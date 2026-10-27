---
title: "Release notes"
description: "What changed in each Caudra release, newest first."
---

Each release lists what changed, newest first. After an update, the first interactive start shows the notes for every release since the one you ran before. Run `/changelog` to read them again, or `/changelog all` for every release. [`ui.changelog_on_update`](/docs/configuration/#ui-changelog-on-update) turns the automatic view off.

To install Caudra, follow the [Quick Start](/docs/quick-start/). To update, run [`caudra update`](/docs/cli/#caudra-update-caudra-rollback). Each release links to its GitHub page, which holds the downloads.

## Caudra 0.2 Preview 16 {#v0-2-0-preview-16}

[v0.2.0-preview.16 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.16)

- The keybindings list filters as you type after `/`, and shows modifiers as compact symbols.
- `F2` opens an empty `/btw`, so a side question no longer needs the command typed first.

### Changes

- **Filter the keybindings list.** In the `F1` list, press `/` and type to keep only the keys whose action, key, or section matches every word. `Enter` keeps the filter and `Esc` clears it. The list is wider, every section starts at the same column, and modifiers show as `⌃`, `⇧`, `⌥`, and `⌘`. Typing `ctrl` or `shift` still finds them. See [Keybindings](/docs/keybindings/).
- **`F2` asks a side question.** Outside a modal, `F2` opens an empty `/btw` that reads the conversation, or the focused task's, when you send. A question you close without sending comes back on the next `F2`. See [Commands](/docs/commands/).

## Caudra 0.2 Preview 15 {#v0-2-0-preview-15}

[v0.2.0-preview.15 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.15)

- Enable 16 of Matt Pocock's engineering skills alongside pstack.
- The main agent can send guidance to a running task or replace its current turn.
- Patch cards stream each file's diff as the model writes it, including the file in progress.
- Content-screening failures say what went wrong instead of labeling the content as flagged.
- Sessions keep subscription spend after a restart and show it beside billed API spend.
- Python snippets support exact decimal arithmetic.
- Model lists remove retired OpenAI and Copilot models and correct prices and limits for Mistral, Z.AI, and DeepSeek.

> **Important**
>
> **Close every other Caudra session before you start Preview 15.** Its first start upgrades the session database in your [state directory](/docs/configuration/#directory-layout) to record content-screening outcomes. If another terminal, editor session over ACP, or `caudra storage` command has the database open, Caudra lists those sessions and waits for them to close.
>
> Updating from Preview 14 saves a copy first at `backups/caudra.db.v39.bak` in the state directory. Preview 14 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary.

### Changes

- **Matt Pocock's skills.** Set `mattpocock = true` under `[plugins.skill]` to enable 16 of [Matt Pocock's skills](https://github.com/mattpocock/skills), adapted for Caudra. Each has a `/mattpocock:<name>` command. They cover plan reviews, domain modeling, test-first development, debugging, code review, specs, tickets, and handoffs. The pack can run alongside pstack, and enabling it takes effect immediately in the terminal UI. See [mattpocock](/docs/skills/#mattpocock).
- **Guide running tasks.** The main agent can send changed requirements or reply to a task's report through `task_control` with `action: "guide"`. The task reads the message at its next step. Adding `interrupt: true` replaces its current turn. Unread guidance comes back with the task's result. See [Inspect and control tasks](/docs/sessions/#inspect-and-control-tasks).
- **Patch previews stream as they are written.** A `file_apply_patch` card shows each file's diff as the model writes it, including the file in progress. Line numbers appear after execution. You still approve the whole patch before any changes are applied.
- **Decimal arithmetic in Python.** The `code` tool supports `decimal.Decimal` for exact calculations. Results keep their decimal type and digits rather than rounding to JSON floating-point numbers.
- **Find sessions blocking an upgrade.** The list of open sessions now includes each session's project directory, so you can find the terminal to close.

### Models

- **OpenAI.** The model list drops the retired GPT-4.1 Nano and o4-mini entries. The `gpt-5.6` alias uses GPT-5.6 Sol's metadata.
- **Copilot.** The built-in list drops GPT-5 mini, GPT-5.4 mini, GPT-5.4, GPT-5.5, and Gemini 3.7 Flash after their October 19 retirement.
- **Mistral.** `zai-glm-5-3-flash` has its own vision-capable entry and prices. Cached input uses the published rates for Mistral Medium, Mistral Small, and Ministral 14B.
- **Z.AI.** `glm-4.7-flashx` uses its paid rates instead of the free Flash model's pricing.
- **DeepSeek.** Flash and V4 Pro allow 393,216 output tokens instead of 384,000.

### Fixes

- **Screening failures no longer look like detections.** When enforced screening cannot check a result, its chip says why, such as `screening timed out` or `engine unreachable`, instead of `flagged`. The `/decisions` overview separates incomplete screening from flagged content. The result stays withheld and the session's restrictions still apply. Older saved rows show the neutral label `screening result`. See [the decision engine](/docs/permissions/#decision-engine-advice).
- **Batched fetches share screening capacity.** Caudra screens at most four chunks at once and shows `waiting to screen` for queued work. A timed-out or unreachable request can retry once within the screening time budget, using the same text without fetching the page again.
- **Session spend stays visible.** Reopened sessions keep their subscription spend. The status bar and `/usage` show it beside billed API spend, as in `$0.01 ≈$9.05`, so a few cents of decision calls no longer hide it. `≈` replaces `~` for spend covered by a subscription. New sessions no longer inherit subscription spend from the previous session.
- **Compaction refreshes reminders.** The agent gets current environment, memory, and model notices after compaction, including after a long idle, rather than waiting for your next message.
- **Cancelling clears decision spinners.** Decision chips no longer keep spinning after you cancel their turn.
- **macOS file-watcher hangs.** Watching and unwatching folders in quick succession no longer hangs the watcher.
- **Go and Rust references.** Initializer calls in grouped Go package declarations belong to the correct variable or constant. Calls in local Go and Rust initializers belong to the enclosing function or method.
- **Expensive SVG icons.** Caudra skips source icons with excessive turbulence filters before rendering them.

## Caudra 0.2 Preview 14 {#v0-2-0-preview-14}

[v0.2.0-preview.14 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.14)

- Clicking the update banner, or running `/update`, installs the new release and restarts Caudra on the same sessions.
- A shell command still running after 10 minutes moves to the background, and tasks and shell commands have one way to run there.
- Commands list their arguments as you type, and dashed commands such as `/goal-clear` became subcommands such as `/goal clear`.
- To-do lists fill in while the model writes them.
- `/catchup` summarizes a session you lost track of, and `/btw` works in subagents chats.
- Decision calls count toward your spend in `/usage`, goal spend, and Autopilot's `max_cost`.
- Long sessions stay fast, and long subagent runs no longer block background results.

> **Important**
>
> **Close every other Caudra session before you start Preview 14.** On its first start, Preview 14 upgrades the session database in your [state directory](/docs/configuration/#directory-layout) to keep each background task's transcript beside its record. While another terminal, editor session over ACP, or `caudra storage` command still has the database open, Preview 14 lists those sessions and does not start. It also moves the global `caudra.toml` to format version 5.
>
> Both upgrades save a copy first: the database as `backups/caudra.db.v38.bak` and `caudra.toml` under `backups/config/global/`, both in the state directory. Preview 13 and earlier cannot open the upgraded database or read the upgraded `caudra.toml`, and `caudra rollback` restores only the previous binary. See [Config file versions](/docs/configuration/#config-file-versions).

### Changes

- **Long shell commands move to the background**: A shell call waited for its command however long it ran. It now waits up to 10 minutes, then the command becomes a background job without a restart, and the agent gets the output so far. The result arrives when the command exits or reaches its `timeoutSec` deadline. Change the wait with `agent.shell_foreground_limit_secs`. `agent.shell_async_threshold_secs` is retired, and Caudra comments it out with the reason. The SDK `init` message reports `shell_execution.foreground_limit_secs` in place of `async_threshold_secs`. See [Execution policies](/docs/sessions/#execution-policies).
- **One background mode for tasks and shell commands**: `agent.task_execution` and `agent.shell_execution` could make every task or shell command wait or return at once. Both are retired, and Caudra comments them out with the reason. A task or shell command runs in the background when the model passes `background: true`, `/tasks background` still promotes a running task, and `--print` always waits. The SDK `init` message no longer has a `task_execution` object. See [Background tasks](/docs/sessions/#background-tasks).
- **Commands guide you through their arguments**: The command list shows each command's arguments, and after the command a list offers values for the slot under the cursor, such as thinking levels, task ids, workflow runs, and docs pages. `Tab` takes the highlighted value, and `Enter` on a mistyped word runs it, so `/thinking hgh` sets `high`. The `Ctrl+P` modal does the same, and Lua plugins add values with the new `complete` option of `register_command`. Commands that were verbs of another command are now its subcommands: `/goal clear`, `/goal model`, `/stash pop`, `/stash list`, `/update dismiss`, `/model next`, `/decisions model`, `/decisions next`, `/sessions move`, `/sessions migrate`, and `/search` in place of `/search-history`. Typing an old name finds the new form. See [Arguments](/docs/commands/#arguments).
- **To-do lists stream as the model writes them**: The `todo_write` card and the sticky to-do panel show each item as soon as the model starts writing it, instead of the whole list once the call ran. If the call fails or is cancelled, the panel returns to the last stored list. ACP clients get a `plan` update as each item is finished. See [Turn updates](/docs/acp/#turn-updates).
- **`/catchup`**: Asks a `/btw` side question that summarizes the session: the goal and where things stand, what needs you, what the agent is doing, and what is done, decided, and still open. Add a few words, such as `/catchup the migration`, to narrow it. Nothing enters history. See [Commands](/docs/commands/).
- **`/btw` in task chats**: In a task's chat, `/btw` and `/catchup` now ask about that task's conversation, and `F2` on a question a task asked clarifies it over that task's conversation instead of refusing. See [Commands](/docs/commands/).
- **Decision calls are priced**: Calls to a decision endpoint showed in `/usage` without a cost. TypeSafe Jev now costs $0.042 and the OpenAI Decisions API on `gpt-6-luna` $0.10 per 1M input tokens, and output is free on both. Decision spend counts toward the session's cost, goal spend, and Autopilot's `max_cost`, including calls that timed out or were cancelled. Other models, such as a local `decision-1`, stay unpriced. See [Decision prices](/docs/providers/#decision-prices).
- **Long background shells ask whether to keep waiting**: Every 30 minutes a background shell runs, Caudra shows the agent waiting on it the latest output and asks whether to keep waiting or cancel, so a command stuck on a prompt or a lock no longer holds the agent until its deadline. Set `agent.shell_wait_nudge_minutes` to change the interval, or to `0` to turn it off. See [Long-running background shells](/docs/context/#long-running-background-shells).
- **Shell results say more**: A result now says how long the command ran, such as `[shell status: exit code 0; took 3m12s]`, and a cancelled command reports its output so far instead of only `Shell execution cancelled`. See [shell](/docs/tools/#shell).
- **More of the composer can be undone**: Clearing the draft with `Ctrl+C`, completing a command, browsing input history, starting a workflow, and applying a saved draft are now undo steps, and undo brings paste chips back with their content. `Ctrl+Shift+Z` redoes in every text field, and `Super+Z` undoes on terminals that report `Super`. See [Text Fields](/docs/keybindings/#text-fields).
- **Recently finished tasks come first**: `/tasks` lists finished subagents newest first, matching `/shells`.

### Fixes

- **Tab completion keeps the rest of the draft**: `Tab` on a command replaced the whole draft with the command name. It now replaces only the command word, so `/goFix the bug` completes to `/goal Fix the bug`. See [Commands](/docs/commands/).
- **A goal set during a turn no longer waits in the queue**: `/goal` or `/autopilot <goal>` sent during a turn became a plain queued prompt, which a reopen could send as bare text. The goal now takes effect at once and starts after the current work. See [Send next](/docs/queue/#send-next).
- **Reopened sessions hold their queued prompts**: Reopening, resuming, or rewinding a session sent its saved queue at once. Saved prompts now wait under **Held until sent** until you choose **Send now**, edit, or delete them. See [Recovery](/docs/queue/#recovery).
- **Long sessions stay responsive**: In a session with many subagents and shell commands, opening a subagent, scrolling `/tasks`, and redrawing got slower the longer it ran. They now stay about as fast as in a new session.
- **Long subagent runs no longer block background results**: A subagent transcript past 8 MiB could not be saved, so Caudra kept showing `Background results could not be saved, retrying` and delivered no more results. Transcripts are now saved in the session database at any size. See [Background tasks](/docs/sessions/#background-tasks).
- **A replaced subagent turn can start background work again**: After you replaced a task's turn from its chat, its new background shells and tasks were refused with `background task admission is closed`. They now start. See [Task chats](/docs/queue/#task-chats).
- **Database upgrades show progress**: An upgrade names each step with a spinner, where a full progress bar made a long upgrade look frozen.
- **Plan cards no longer flicker**: A plan card's preview briefly disappeared before its result arrived, which made the transcript jump.
- **Update from the banner**: The update banner only named a command to run in another terminal. Clicking it, or running `/update`, now asks whether to update, installs the release in the terminal, where a sudo prompt can reach you, and restarts Caudra on the same tabs with the same flags. Running work is interrupted, and `agent.resume_interrupted` decides whether it resumes. On Windows and in an `--ephemeral` run, Caudra exits instead of restarting. A Homebrew, Nix, or Cargo installation says to update through its package manager. See [ui.update_check](/docs/configuration/#ui-update-check).

## Caudra 0.2 Preview 13 {#v0-2-0-preview-13}

[v0.2.0-preview.13 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.13)

- Work that a crash, a kill, or quitting cut off resumes when the session opens again.
- Autopilot is no longer experimental, and engaging it tells you what it runs without.
- JavaScript and TypeScript `code` snippets run as Python when every part of them translates without changing what it does.
- Memory recall follows the work in progress and reaches subagents, and decision waits show a timer.
- A task chat can replace the child's current turn, and `/fork` forks the session at its latest message.
- Shell ETAs read the command itself, and `/shells` shows them with each command's run time.
- pstack picks its models from your favorites and `/settings`, and its statechart tool runs inside Caudra.
- TLA+ files are highlighted and outlined, and Mermaid state diagrams are drawn in the chat.

> **Important**
>
> **Close every other Caudra session before you start Preview 13.** On its first start, Preview 13 upgrades the session database in your [state directory](/docs/configuration/#directory-layout) to rebuild the log of shell runs that shell ETAs learn from. While another terminal, editor session over ACP, or `caudra storage` command still has the database open, Preview 13 lists those sessions and does not start. It also moves the global `caudra.toml` to format version 4, which it writes to disk only while no other Caudra process runs.
>
> Both upgrades save a copy first: the database as `backups/caudra.db.v37.bak` and `caudra.toml` under `backups/config/global/`, both in the state directory. Preview 12 and earlier cannot open the upgraded database or read the upgraded `caudra.toml`, and `caudra rollback` restores only the previous binary. See [Config file versions](/docs/configuration/#config-file-versions).

> **Important**
>
> pstack no longer reads `~/.config/caudra/pstack/models.md`, and its `setup-pstack` skill is gone. If you wrote that file, choose your models again in `/settings` → **pstack**, then delete the file.

### Changes

- **Interrupted work resumes on reopen**: When Caudra crashed, was killed, or was quit while a session worked, reopening it left the work stopped. Running tasks showed as interrupted or cancelled and never reported, and an open shell call said only `[Tool result not available]`. Now the main turn and every agent task the main chat started resume at once, each task under its own task ID and in the background. Every resumed agent first reads a "Resumed after interruption" reminder that lists what was cut off and asks it to check its effects before carrying on. Shell commands are not run again, and a cut-off command's result says its outcome is unknown. `[agent] resume_interrupted` (`auto`, `ask`, or `off`) chooses what happens. `auto` asks instead after a resume already crashed once, after `resume_max_age_hours` (default `24`), or while automatic wakes are suppressed. ACP clients get the resume on `session/load`, with what was cut off in `_meta.caudra.interrupted`. Quitting also no longer records a running task as cancelled with `background task admission is closed`. See [Resuming after a restart](/docs/sessions/#resuming-after-a-restart).
- **Autopilot is no longer experimental**: `/autopilot`, `--autopilot`, its status chip and panel, the SDK controls, and the `autopilot` view in automations no longer need `autopilot = true` under `[experimental]`. Autopilot still engages only when you ask, and the `[autopilot]` limits are unchanged. Caudra comments out an `experimental.autopilot` line with the reason. Engaging now adds a notice when no decision endpoint is active or content screening is off, since nothing then keeps a fetched page from steering an approval, and when the model bound to the Autopilot job cannot be used. Each notice says what to do, and autopilot engages either way. SDK clients receive a `caveat` event, and `-p` prints the notice to stderr. See [Before you leave it alone](/docs/autopilot/#before-you-leave-it-alone).
- **JavaScript snippets run as Python**: A `code` snippet written in JavaScript or TypeScript, such as `const r = await Promise.all([...]); return r.map(x => x.name)`, was rejected with `This looks like JavaScript`, and the model spent a turn rewriting it. Caudra now translates it to Python and runs that, when every part has a Python form that keeps its meaning. The card and the model see the Python that ran. A snippet with a part that has no such form, such as `switch`, `if (items)` on a bare value, or a server call that is never awaited, is still rejected, and the error names that part and its line. See [Results and errors](/docs/mcp/#results-and-errors).
- **Memory recall follows the work**: `memory_recall` judged notes against your message alone, so "commit this" or "go" could bring up notes about other tasks, and it ran only at the start of a turn in the main session. It now also weighs the to-do in progress, up to 3 open to-dos, and your previous request when the new one is short. It runs again when a to-do step starts, when a goal check finds the goal unmet, and after compaction, at most once every 5 model requests for steps and goal checks. Rare words count for more than common ones, a word matches its other forms, such as "resizing" and "resize", and notes the conversation already read are left out. Subagents recall too. Recall has its own `thresholds.recall_confidence`, default `0.75`, instead of `routing_confidence`. See [Relevance from the decision engine](/docs/memory/#relevance-from-the-decision-engine).
- **Decision waits show a timer**: Memory recall, skill suggestions, the goal prescreen, the question nudge, and compaction retention waited on the decision engine with nothing on screen. Each now shows a chip on the message, to-do card, goal check, or compaction notice that started it, with a spinner and the time so far, and the chips on tool cards count up the same way. A recall names why it ran, such as `memory_recall · step started · no notes`. A decision that changed nothing leaves a dim `checked` chip. Click a chip for the details, including what came closest to the threshold, and click again to fold them. See [Decision engine](/docs/configuration/#decisions).
- **Replace a task's turn from its chat**: A task chat could only guide the child, so a task heading the wrong way kept going until it finished or you cancelled it. `Ctrl+X x`, the `replace` control, `r` on a guidance row, or **Replace current run** in its menu now stops the child's current turn, with its tool calls, nested tasks, and background jobs, and runs it again on the same history with your message. The agent that started the task gets the result of the new turn. See [Task chats](/docs/queue/#task-chats).
- **`/fork`**: Copies the session into a new one at its latest message, the same as **Fork here** in that message's `⋮` menu. During a turn, the fork stops before the prompt that started it and puts that prompt in the new session's composer. See [Message actions](/docs/sessions/#message-actions).
- **Shell ETAs read the command**: The shell ETA model now reads each command for what decides its time, such as `sleep` and `timeout` values, loops, chains, pipes, programs, and flags, and adds up a new chain from the times of its parts. In the shell runs recorded on a test machine, it gave a right ETA for three in five commands that ran 10 seconds or more, up from under a third, with fewer wrong ones. The first shell call of each process waits a second or two while the model trains. `/shells` now shows a running command's ETA and a clock, such as `ETA ~40s · 12.3s`, and a finished command's run time. See [`agent`](/docs/configuration/#agent).
- **pstack chooses models you can run**: pstack's pages named Claude Opus and Grok for every role, so a session without Grok had its tasks rejected, and a review panel ran one model twice. The pages now name six kinds of work, such as Code, Judgment, and Panel, and end with a **Model roles** table of ready `task` arguments. Code work runs on your Subagent model, judgment on your newest flagship favorite, and a panel on flagship favorites from different providers. Without favorites, Caudra takes the flagship of each provider you are signed in to. The **pstack** tab in `/settings` changes a kind's model and thinking. See [Model roles](/docs/skills/#model-roles).
- **pstack's statechart runs without Node or Java**: `/pstack:statechart` is now `pstack.statechart(...)` in the `code` tool, with the same `from-spec` and `from-chart` commands, plus `compare` for two charts, and checks specs with tla-checker inside Caudra. A spec's mapping is now a Rhai `SPEC.map.rhai` instead of `SPEC.map.mjs`, and charts are Mermaid `.mmd` only, so `--png` is gone. See [pstack scripts](/docs/code/#pstack-scripts).
- **TLA+ support**: `.tla` files and `tla` code blocks are highlighted, with PlusCal inside `(* --algorithm ... *)` comments, and `tlacfg` blocks highlight TLC configs. `file_index` outlines specs, and `source_code_refs` on an action finds the `Next` that takes it. See [Code Intelligence](/docs/tools/#code-intelligence).
- **Mermaid state diagrams are drawn**: A `stateDiagram` block, such as a chart from pstack's statechart tool, is drawn like a flowchart instead of shown as code. A transition from a state back to itself is drawn in both kinds of diagram. See [Diagrams](/docs/markdown/#diagrams).

### Fixes

- **Background tasks outlive a failed or cancelled turn**: When the main turn failed or was cancelled, a running subagent or background shell kept running but its card stopped updating and its result never reached the agent. Its card now keeps updating, and its result arrives with your next message.
- **Background work survives a brief database lock**: A brief database lock no longer stops goal or autopilot sessions from continuing after background tasks finish, or cancels running subagents' shells. Caudra retries, and says so in the chat if saving keeps failing.
- **A batched call's decision opens with that call**: A decision on a call inside a batch was drawn open at the top of the batch card. It is now a chip on that call's row.
- **A stray indent no longer makes a server call unconditional**: Preview 12 lined up an over-indented line with its block. When that block names a server or a host function, the indent may be all that is left of a missing `if` or loop, so such a snippet is rejected with `Unexpected indentation` again.
- **No stale quota rows**: An OpenAI account no longer shows a `secondary 100% left` row for a limit its plan does not have, and a window a provider stops reporting disappears.
- **A disabled MCP server stays disabled**: A server with `enabled = false` and an invalid entry showed as failed and counted as enabled. It is now disabled like any other.
- **Moving a session works while another Caudra is open**: Moving a session could abort with `invalid database value in checkpoint log frames: -1` while another Caudra process tidied the database. Caudra now treats this as a busy database, and the move goes ahead.
- **JSON keys have their own colour**: Keys in highlighted JSON took the string colour. They now take the theme's member colour, as YAML and TOML keys do.

## Caudra 0.2 Preview 12 {#v0-2-0-preview-12}

[v0.2.0-preview.12 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.12)

- Caudra learns from your conversations in the background and keeps what it learned in your instruction files and project memory.
- pstack's engineering skills come built in, off until you turn them on.
- Config files upgrade themselves to a new format, with a backup first and a note on every line they change.
- `/settings` edits `caudra.toml`, `websearch` follows the backends you choose, and `webfetch` reaches the local servers you list and reads Reddit again.
- More lookups rest as one row that says what they found, and a `code` card folds a printed dict or list to a tree.
- Cards say how many tokens a result adds, with estimates that learn each model's tokenizer from what its provider billed.
- Shell cards show an ETA from a small model that learns from your own past commands, with no decision endpoint.
- Decisions show on the tool call's card, apply as soon as you save them, and content screening can withhold a flagged result.

> **Important**
>
> **Close every other Caudra session before you start Preview 12.** On its first start, Preview 12 upgrades the session database in your [state directory](/docs/configuration/#directory-layout). The upgrade needs exclusive access, including from other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 12 does not start and changes nothing. It lists the sessions to close instead, with each one's process ID and title.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `backups/caudra.db.v32.bak` in the state directory. Preview 11 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary.

> **Warning**
>
> **Before you start Preview 12, close older Caudra sessions so it can upgrade your config files.** `caudra.toml` moves to format version 3, and `mcp.toml` and `permissions.toml` to version 2. Preview 12 rewrites the global files when it starts, after it saves a copy of each, but only while no other Caudra process is running. Otherwise it reads them upgraded in memory and leaves them unchanged on disk, and saving to them from inside Caudra, such as with `/env` or `/mcp`, fails until Caudra starts with no other session open.
>
> Preview 11 and earlier cannot read an upgraded file. To go back, copy the file's backup from `backups/config/global/` in the state directory over it. Files in a project's `.caudra/` are never rewritten on their own. If you commit them, run `caudra config migrate --project` once everyone who shares them runs Preview 12. See [Config file versions](/docs/configuration/#config-file-versions).

### Models

- **MAI-Code-1.1-Flash on GitHub Copilot.** `copilot/mai-code-1.1-flash` and `copilot-enterprise/mai-code-1.1-flash` now have their price and size class before Copilot's model list arrives: $0.20 per million input tokens and $1.20 per million output tokens. Caudra counts it among the Fast models. See [Copilot](/docs/providers/#copilot).
- **GPT-6 Astra and Claude Fable 5.1 on GitHub Copilot.** Both cost $10 per million input tokens and $50 per million output tokens on Copilot. Caudra used to bill Fable 5.1's cache reads at Fable 5's $1 instead of $0.25, so a Fable 5.1 session's cost in `/usage` comes out lower from now on.

### Changes

- **Continual learning**: Caudra now learns from how you work with it, in the background. After at least 10 messages you typed and 2 hours since the last run, once the session has been idle for a minute, a read-only learner on the new **Learn** model job reads the project's recent conversations and checks each finding against the project's history. Caudra writes what will still hold to a managed block in the project's `AGENTS.md` (shared conventions), `AGENTS.local.md` (your machine's setup, which Caudra keeps in `.gitignore`), `~/.config/caudra/AGENTS.md` (your own preferences), or project memory notes. It never edits text outside its block. The status bar says when a run changed something. `/learn` lists the runs with what each changed, opens a run's transcript with `t`, runs one now with `n`, reverts a change with `u`, and pauses the project with `p`. It is on by default. Turn it off with `[agent] continual_learning = false`, or narrow where it writes with `learning_targets`. Its spend shows under `learning` in `/usage`. See [Continual learning](/docs/learning/).
- **More lookups fold like a file read**: `history`, `tool_output`, `skill`, `host.environment`, `list_sessions`, `read_topic`, and `automation` calls, and the `plan`, `workflow`, and `task_control` calls that only read, now rest as one row that says what they found, such as how many hits a search kept or a run's state. A click opens the whole answer. Calls that change something keep their cards open. The new `plan_read`, `workflow_read`, and `task_control_read` entries of the default `ui.always_collapsed` list cover the reading calls. If you set your own `always_collapsed` list, add the entries you want from the [default](/docs/configuration/#ui).
- **Printed dicts and lists fold to a tree**: When a `code` snippet prints or returns a dict or a list, the card shows it as a folded tree, such as `▸ {…} 7 keys`, and a click opens or folds any part of it. The raw view of an MCP call's card draws its JSON as the same tree. When the tree only repeats one call's answer, such as `print(await host.environment())`, that call's card shows open instead. The model still reads the output as text. See [In the transcript](/docs/code/#in-the-transcript).
- **Clearer card titles**: A skill's card reads `Skill loaded <name>`. `task_control`, `workflow`, and `automation` cards have their own titles with the action as a verb, such as `◷ Jobs checked shell-66` or `⑂ Workflow started review`, in place of the tool's name and raw action. Titles set the names and paths they show in italics.
- **Cards say what a result costs the context**: A card whose result the model reads now estimates its tokens beside what it found, such as `209 lines · ~2.1k tokens` on a file read. Caudra counts tokens locally with OpenAI's tokenizer, and now learns from what each provider billed how far that count is off for each model, and keeps it across sessions. Card counts, the `/context` breakdown, the tool ledger in `/usage`, and automatic compaction all use what it learned. See [Watching it work](/docs/token-economy/#watching-it-work).
- **`code` cards keep a clock**: A `code` card's header counts up while the snippet runs and then shows how long it took, such as `· 2.4s`, as a `shell` card does. A run that slept no longer shows a stray separator, and its `batch` row keeps what it found. See [In the transcript](/docs/code/#in-the-transcript).
- **Time and usage move into the `⋮` menu**: Tool card headers no longer end in the time and the request's usage. The first line of a message's `⋮` menu shows them, with the date when the message is not from today, for every prompt, reply, reasoning block, and tool call, in restored sessions too. A running turn's messages and a subagent's transcript have the menu as well. See [Message actions](/docs/sessions/#message-actions).
- **Choose your web search backends**: `websearch` used Exa's hosted search no matter what you set. It now follows `WORKCELL_WEBSEARCH_BACKEND` and `SEARXNG_URL`, and the new `[websearch]` table in the global `caudra.toml`. List several backends in `backends`, such as `["searxng", "exa-mcp"]`. In the default `fallback` mode, the next one answers when one fails. `single` uses only the first, and `all` lets the model pick one per call. Edits apply at once in every open session of the terminal UI. With nothing set, `websearch` uses Exa's hosted search as before. See [Web search](/docs/web-search/).
- **Edit `caudra.toml` from `/settings`**: `/settings` has a Web search tab that shows whether each backend is ready and lets you set up, reorder, and switch them, and tabs for the top-level keys, `[ui]`, `[agent]`, the builtin skills, `[provider]`, `[storage]`, `[worktrees]`, `[autopilot]`, and `[automations]`. Switches, value chips, and list editors change a key, and `/` filters every tab. The pane under the list explains the selected key, its default, and when an edit applies, and `?` opens its section of the configuration reference. It writes the global `caudra.toml` and keeps your comments. A key your project's `caudra.toml` also sets is marked, since the project value wins. See [Configuration](/docs/configuration/).
- **Decision settings apply as soon as you save them**: On the Features tab of `/decisions`, click a feature's mode or press `Space` or `Enter` to move to its next, such as `off`, `shadow`, then `enforce`. Caudra saves it under `[decisions.features]` in the global `caudra.toml`, keeping your comments. An edit to `[decisions]` or `ui.show_decisions` in `caudra.toml`, to the active decision endpoint's table in `providers.toml`, or to `decisions/permission.json` now applies at once in every open session, without `/reload`. A project `caudra.toml` that turns a feature off still wins. See [the decision engine](/docs/permissions/#decision-engine-advice) and [Hot reload](/docs/configuration/#hot-reload).
- **Decisions about a call ride on its card**: A decision about a tool call, such as a `source_code_context` rerank or Auto screening, no longer draws its own row under the card. The card's header shows a chip, such as `◆ reranking…` and then `◆ reranked`, and opening the card shows the decision and its scores. Chips that warn, such as `◆ asked you`, use the warning colour. Decisions about a turn, such as a memory recall, keep their rows. See [Decision rows in the transcript](/docs/permissions/#decision-rows-in-the-transcript).
- **Rerankers judge a wider pool in one request**: With a [decision endpoint](/docs/providers/#decision-endpoints), a rerank now sends up to 64 candidates in one request instead of 16 at a time. `memory_search`, `history_search`, and `source_code_context` judge up to 64, and in `enforce`, `source_code_context` keeps only the rows at or above `thresholds.relevance_floor` unless none clears it. A request waits one `rerank_timeout_ms` for every 16 candidates, and `rerank_timeout_ms` now defaults to 3000 instead of 2000. A workflow run paused in the middle of a `rerank()` on an older release fails when it resumes. See [Reranking](/docs/configuration/#reranking).
- **Decision requests wait up to 10 seconds by default**: `[decisions] timeout_ms` now defaults to 10000 instead of 800, so a busy or local engine no longer runs out of time and leaves a feature without an answer. A slow engine can now hold a screened tool call, or the start of a turn for memory recall, for up to that long. Set `timeout_ms` lower to fall back sooner. See [`decisions`](/docs/configuration/#decisions).
- **Content screening can withhold a result**: The new `content_screening = "enforce"` keeps a flagged `webfetch`, `websearch`, `code`, or MCP result from the model, which reads a notice in its place asking it to tell you. Caudra deletes the result's saved copy, so `tool_output` cannot read it back. The card keeps the result for you, marked `Decision engine: withheld from the model`. Enforce fails closed: a result is also withheld when the engine fails or times out, or when part of it could not be screened. The feature still defaults to `off`. See [the decision engine](/docs/permissions/#decision-engine-advice).
- **Shell ETAs from your own runs**: A shell card's ETA now comes from a small model that runs on your machine and learns from the commands you run, in place of the `shell_duration` decision feature. It needs no decision endpoint and sends nothing anywhere. A command like one you ran before shows how long that took, such as `ETA ~40s`, and others show `ETA seconds` or `ETA minutes` when the model is confident. It only labels the card and changes no timeout. On its first start, Preview 12 trains it from the newest 20,000 shell calls your saved sessions hold, keeping only hashes of each command's words, and the first shell command after Caudra starts waits while it trains. It is on by default. Turn it off with `[agent] shell_eta = false`. The `shell_duration` feature and its thresholds are gone, and the config upgrade comments out those keys. See [`agent`](/docs/configuration/#agent).
- **pstack built in**: Lauren Tan's [pstack](https://github.com/cursor/plugins/tree/main/pstack) skills for Cursor now ship with Caudra under their MIT license, with the `deslop`, `control-cli`, and `control-ui` skills from Cursor's team kit. Turn them on with `[plugins.skill] pstack = true`. The agent then sees one `pstack` skill whose index lists every skill and whose pages load by address, such as `pstack/poteto-mode`, translated to name Caudra's tools, paths, and commands. Each skill you run directly gets a `/pstack:<name>` command, pstack's task profiles appear, and its scripts become `pstack` calls in `code`, ported to Rust so they need no Bun, Node, or Bash. If `gh` keeps its login in `GH_TOKEN` or a keyring, forward those variables with `/env`. Benny's Slack triage and watch-pr's drive mode ship as [automations](/docs/automations/): with `experimental.automations` on, they are listed in `/automations` under the new `bundled` scope, ready to arm. See [pstack](/docs/skills/#pstack).
- **`webfetch` reaches the servers you list**: `webfetch` refuses private, loopback, and carrier-grade NAT addresses, so it could not read a local dev server or a machine on your tailnet. The new `[web]` table in the global `caudra.toml` lists the ones it may reach in local sessions. `allow_private_hosts` takes exact `HOST:PORT` entries, such as `"localhost:3000"`, and `allow_private_networks` takes `loopback`, `private`, `cgnat`, `tailscale`, or a CIDR. When `webfetch` refuses a private target, the error names the `HOST:PORT` to list. Remote workspaces and sandboxes keep the public-internet rule. See [`[web]`](/docs/configuration/#web).
- **`webfetch` reads Reddit**: Reddit answers page fetches with a script challenge, so `webfetch` could not read any post. It now reads threads, subreddits, user pages, and searches from Reddit's feed or JSON API, and gives the model the post and its comments as Markdown. A rate-limited fetch on any site that sends `Retry-After` now says how many seconds to wait.
- **Config files upgrade themselves**: Every TOML config file now has a format version in its top-level `version` key. When a release changes a format, Caudra upgrades your files and keeps your layout and comments, with a comment above each line it changes, such as `# Upgraded by Caudra 0.2.0-preview.12 on 2026-10-20: renamed ...`. A removed key is commented out rather than deleted. A notice says when a project file is behind. `caudra config migrate` upgrades the global files on demand, `--project` adds the project files, and `--dry-run` shows a diff without writing. See [Config file versions](/docs/configuration/#config-file-versions).
- **Old keys are upgraded instead of refused**: Keys from earlier releases that stopped Caudra with an error, or silently stopped matching, now move to their new form when the file is upgraded. For example, `code_context` and `code_refs` become `source_code_context` and `source_code_refs` in `caudra.toml` and `permissions.toml`, `[python_execution]` becomes `[code]`, and `[bash]` becomes `[shell]`, so rules written under those names apply again. Removed keys, such as `agent.defer_builtin_tools` and the top-level `defer_tools` in `mcp.toml`, are commented out. A file that sets both an old key and its new name is left unchanged, with an error that names both. The legacy decision endpoint keys in `[decisions]`, such as `base_url`, still stop Caudra with the error that shows the `providers.toml` table to write.
- **Backups have their own folder**: Copies Caudra saves before it changes a file now go to `backups/` in the state directory, such as the session database before a schema upgrade and each config file before an upgrade. Backups are owner-only and never deleted. Database backups from earlier releases stay beside `caudra.db`. See [Backups](/docs/configuration/#backups).

### Fixes

- **Content screening reads the whole result**: It read only a few samples from the start of a result, so an instruction in the middle of a long page was never checked. It now reads the whole result in overlapping chunks.
- **Switching decision endpoints keeps flagged content flagged**: A session whose content screening had flagged web or MCP output lost that flag when you switched the decision endpoint. It now keeps it.
- **Every task card names its model**: Every task card and `batch` row now names the model its subagent runs on, while the task runs, after it finishes, and after a reload.
- **No false "statement of intent" nudge while work runs**: The nudge that a turn ended on a statement of intent now waits until no task, background job, or workflow is running.
- **Claude models on GitHub Copilot cache their prompts**: Claude models through `copilot` and `copilot-enterprise` never marked where the prompt may be cached, so every turn sent the whole conversation as fresh input. They now mark it the way the `anthropic` provider does, so later turns read the conversation from the cache.
- **Copilot Enterprise takes a tenant name**: A GitHub Enterprise hostname without a dot, such as `company`, now means `company.ghe.com`.
- **`/login` says why it failed**: A failed sign-in from the TUI now carries the reason the login gave.
- **Caudra starts when a provider's credentials are gone**: If the model you last used belongs to a provider whose saved login was removed, Caudra now starts and opens `/login` instead of stopping.
- **One token reads `~1 token`**: Card sizes said `~1 tokens`.
- **The selected row in `/decisions` is readable**: In the dark themes, the whole selected row now takes the highlight's colours.
- **Background commands keep their clock**: A `shell` command sent to the background now counts its time while it runs, then shows how long it took and its final state, such as `succeeded`, even after you reopen the session.
- **A stray indent no longer stops a `code` run**: A snippet with a line indented deeper than the one before it no longer fails with `Unexpected indentation`. Caudra lines the line up with its block, and the card lists the fix. See [Results and errors](/docs/mcp/#results-and-errors).

## Caudra 0.2 Preview 11 {#v0-2-0-preview-11}

[v0.2.0-preview.11 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.11)

- Sign in to one provider with several subscriptions and API keys, and let Caudra switch between them without breaking the prompt cache.
- Config edits keep your comments.
- Opt-in cache warming keeps the Anthropic prompt cache alive through long tool runs.
- Editors and desktop apps get more of Caudra over ACP: several sessions, session management, sign-in, steering, and background subagents.
- Image generation, the host description, workflows, and automations move into the `code` tool as modules with on-demand help, and `tool_search` is gone.
- Snippets can sleep and read the clock, and `timeoutSec` bounds a run.

> **Important**
>
> **Close every other Caudra session before you start Preview 11.** On its first start, Preview 11 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them has the database open, Preview 11 does not start. It lists the sessions to close, and nothing is changed.
>
> The upgrade keeps your sessions, saves the old database as `caudra.db.v30.bak` next to `caudra.db`, and moves each saved provider login to `auth/accounts/<provider>/default.json`. Preview 10 and earlier cannot open the upgraded database or read the moved logins, and `caudra rollback` restores only the previous binary.

> **Warning**
>
> **Before you start Preview 11, remove the settings that deferred tools.** `image_generate`, `execution_environment`, `workflow`, and `automation` are now [modules of the `code` tool](/docs/code/#tools-reached-from-code), and `tool_search` is gone. A config or profile that still uses any of the following fails to load, with an error that names the fix:
>
> 1. Delete `defer_builtin_tools` from `[agent]` in `caudra.toml`.
> 2. Delete `tool_search` from `[decisions.features]`.
> 3. Replace `lazy` in a [system prompt profile](/docs/system-prompts/#choose-tool-availability)'s `tools` block with `eager` or `disabled`. An MCP tool that was `lazy` stays callable as `eager`.
> 4. Remove `code` from `agent.disabled_tools` and from profile overrides that disable it. `code` now hosts the modules and cannot be turned off, so turn off a module's tool group instead: `images` for `image`, `execution` for `host`, and `delegation` for `workflow` and `automation`.
>
> Permission rules, `agent.disabled_tools` entries, and profile groups that name the four tools keep working. In snippets, `decide()`, `rerank()`, `search()`, `servers()`, `resources()`, and `read_resource()` are now `decisions.decide()`, `decisions.rerank()`, `mcp.search()`, `mcp.servers()`, `mcp.resources()`, and `mcp.read_resource()`, and the old names raise `NameError` naming the new call.

### Code

- **Modules with on-demand help**: A `code` snippet now reaches everything beyond the interpreter through modules: `history`, `mcp`, `decisions`, `image`, `host`, `workflow`, and `automation`. `help()` lists them, and `help("image")` or `help("workflow.start")` shows the full reference with examples. The first `history.search`, `history.read`, `image.generate`, or `workflow.start` in a conversation returns its reference instead of running, unless `help` already showed it. The `code` description no longer changes with what the session connects, so the prompt cache holds. See [Modules](/docs/code/#modules).
- **Tools as module calls**: `await image.generate(prompt, out)`, `await host.environment()`, `await workflow.start(...)`, and `await automation.list()` run the tools they replace, with the same permission rules, plan mode checks, and cards, and return dicts to the snippet. Each call's row on the `code` card opens to the tool's own card. See [Tools reached from `code`](/docs/code/#tools-reached-from-code).
- **Sleep, clocks, and `timeoutSec`**: `time.sleep()` and `await asyncio.sleep()` now wait for real, and `time.time()`, `time.monotonic()`, and `datetime.now()` read the host clock in UTC. Waiting does not count toward the 30 seconds of computation, and `Esc` stops it. `timeoutSec` is back as a wall-clock deadline for the whole run, 120 seconds by default and up to 3600, so a snippet can poll a workflow. See [The Python subset](/docs/code/#the-python-subset) and [Run time](/docs/code/#run-time).
- **More Python**: Monty 1.2 adds CSV and TOML parsing, hashing, bisection, `os.path`, match statements, and dataclass field defaults.
- **JavaScript habits repaired**: A snippet that starts an assignment with `const`, `let`, or `var`, or passes a host call one JavaScript object, now runs as the Python it meant. See [Results and errors](/docs/mcp/#results-and-errors).
- **What a subagent did, on its task card**: A running task card now lists every step of the subagent, newest last, instead of only the current one. A `code` row reads like `code 3 calls` and lists the newest calls with their status. See [In the transcript](/docs/code/#in-the-transcript).

### Accounts

- **Several accounts per provider**: Each `caudra auth login` adds an account, such as a second Claude subscription or an API key, and `--name` names it. `caudra auth status` lists every account with the quota it has left, `caudra auth use <provider> <name|auto>` chooses one, and `caudra auth rename` and `caudra auth logout <provider> [name] [--all]` manage them. `--account <provider>:<name|auto>` and `CAUDRA_ACCOUNT` choose an account for one run. Several comma-separated keys in a provider's key variable become the accounts `env-1`, `env-2`, and so on. See [Accounts](/docs/accounts/).
- **Automatic switching**: With two or more accounts, Auto keeps each conversation on its account and moves it to another before the account runs out. It moves only where the prompt cache is cold anyway, unless the account is nearly spent or refused, and a rate limit shorter than 30 seconds waits on the same account. `ordered` tries the accounts in priority order, and `paced` uses the quota that resets soonest. `[<provider>.switching]` and `[[<provider>.accounts]]` in `providers.toml` set the thresholds, the order, and which API keys may take over from spent subscriptions. See [How Auto switches](/docs/accounts/#how-auto-switches).
- **Across providers**: With `[switching] providers = ["anthropic", "openai"]` in `providers.toml`, Auto moves a conversation to another provider's subscription once every subscription of its own runs out, and back once the first provider has quota again. See [Across providers](/docs/accounts/#across-providers).
- **Accounts in the TUI**: `/accounts` or `Ctrl+X i` lists, pins, adds, renames, orders, and removes the accounts of every provider, with fresh quota readings. The status bar shows the account a conversation uses, each switch adds a transcript row with its cause and cache cost, and `/usage` splits spend by account. `/login` also offers the xAI subscription sign-in.
- **Sign-in details**: A key saved by `caudra auth login` is now tried before the provider's key variable, and the prompt hides the key as you type it. `caudra auth logout` deletes only the credential and keeps the account entry in `providers.toml`.

### Cache warming

- **Opt-in cache warming**: `cache_warming = "running"` under `[agent]` in the global `caudra.toml` sends the last request again with `max_tokens: 0` shortly before its Anthropic prompt cache expires, while a run waits on a long tool, subagent, question, or permission prompt. `"idle"` also warms for up to 30 minutes after a run ends. It is off by default, covers the Anthropic API only, and warms only when that costs less than the cache write it avoids. `/usage` shows what warming cost and saved, or what it would have saved while it is off. See [Cache warming](/docs/token-economy/#cache-warming).

### ACP

- **Several sessions**: One `caudra acp` process now keeps every session an editor opens, each with its own tools, so sessions in different projects run side by side. Editors can list, resume, close, delete, and fork Caudra sessions, and the list shows each session's title and its first and last prompt. See [ACP](/docs/acp/) and [Sessions](/docs/acp/#sessions).
- **Sign-in**: Editors get a `caudra-login` auth method that runs `caudra auth login`, and `caudra acp` starts with no signed-in provider so the editor can offer it. See [Authentication](/docs/acp/#authentication).
- **Pickers and model catalog**: Editors get pickers for reasoning effort, mode, and permission mode beside the model, and a fast mode toggle where supported. The choices are saved with the session, and editors can list models before opening one. See [Config options](/docs/acp/#config-options).
- **Slash commands and compaction**: Editors list `/compact`, your skills, and MCP prompts as slash commands, and show compaction progress and retried provider requests. See [Slash commands](/docs/acp/#slash-commands).
- **Cancellation and steering**: Cancelling a prompt withdraws open permission prompts and questions and answers `cancelled` exactly once. A prompt sent right after a cancel runs once the cancelled turn ends, so editors such as Orca that steer by cancelling and prompting again work, and a prompt sent with `_meta.caudra.delivery: "steer"` joins the running turn. See [Prompts, cancellation, and steering](/docs/acp/#prompts-cancellation-and-steering).
- **Turn details**: Answers carry the turn's duration and usage, tool calls name their tool, shell calls report stdout, stderr, and exit code, and failures carry their text instead of a bare "Internal error". See [Turn updates](/docs/acp/#turn-updates).
- **Subagents and background tasks**: Subagents and shell commands can run in the background over ACP, as in the TUI. Editors that support ACP subagent sessions get one per subagent, editors that opt in with `_meta["caudra/childWork"]` can follow and stop this work, and other editors see each subagent's tool calls in the parent session. See [Subagents and background tasks](/docs/acp/#subagents-and-background-tasks).
- **Questions as permission requests**: An editor without form elicitation that sets `_meta["caudra/questionsViaPermission"]` gets the `question` tool's questions as permission requests. See [Permissions and questions](/docs/acp/#permissions-and-questions).
- **Answers every request**: A second prompt to a busy session is refused instead of left unanswered, and a stdin line that is not JSON or is over 64 MiB is skipped instead of breaking the connection.

### Changes

- **Image viewing**: The image viewer and workbench image tabs now zoom and pan. Pictures are sized to the terminal's cells and redrawn when the window or font changes, and a click outside the picture closes the viewer.
- **Orca resumes Caudra**: In an Orca terminal, Caudra now reports its session id, so Orca can reopen it with `caudra --resume <id>`, and lists running subagents as child rows. Off Windows, clicking a row switches to that subagent's chat when Orca supports it. See [Orca](/docs/notifications/#orca).
- **Docs from any modal**: A `?` on the top border of a modal such as `/accounts` or `/usage`, or `F1`, opens the manual section about it above the modal, so `Esc` returns to it. The key hints on the top border of `/usage` and `/tools` also answer a click. See [Docs](/docs/commands/#docs).
- **Tool cards fold like a file read**: `source_code_context`, `source_code_refs`, and `memory` reads now rest as one row that a click opens whole. A [Home Assistant](/docs/mcp/#home-assistant) or [Executor](/docs/mcp/#executor) call inside a `code` or `batch` card shows its whole card, a `memory` write shows the whole note, and a click on a write's title folds it in the expanded `/view` too. If you set your own `ui.always_collapsed` list, add `"source_code_context"`, `"source_code_refs"`, and `"memory_read"` to it.
- **One proxy for every request**: All of Caudra's own requests, decision endpoints and Lua's `caudra.net` included, now use `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, and `NO_PROXY`. A proxy set only in a project `.caudra/.env` no longer applies, so set it in your shell or the global `.env`. See [Network proxies](/docs/cli/#network-proxies).

### Fixes

- **Cancelling stops the running command**: Cancelling a turn while a `shell` command runs, with `Esc` or from an ACP editor, now stops the command and every process it started.
- **Config edits keep your comments**: Saving a provider or decision endpoint, toggling an MCP server, or saving sandbox records now changes only the keys that changed, and keeps your comments, quoting, and table order.
- **Refusals end the turn**: A provider refusal or content filter now ends the turn as refused instead of prompting a retry.
- **Resources read as their type**: An MCP resource that is not markdown now shows its text as the server wrote it.
- **Purposes sorted by spend**: The session's Per purpose table in `/usage` now lists the purpose that cost the most first.

## Caudra 0.2 Preview 10 {#v0-2-0-preview-10}

[v0.2.0-preview.10 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.10)

- One `code` tool replaces `mcp` and `python_execution`, and can ask the decision engine to judge and rerank results.
- Search past conversations, from the `code` tool and from the TUI with `/search-history`.
- Two source code tools instead of five, renamed `source_code_context` and `source_code_refs`, that every model sees and that list call sites.
- Decision models in `/login` and `/model`, Auto mode out of experimental, and workflow `decide()` in the OpenAI Decisions question shape.
- Pictures in the transcript.
- Notes on your answers to the agent's questions, and drag to copy in `/btw` and the question form.
- Goals stop when they make no progress, wait for CI or a deploy, and tell automations when they are stuck.
- Subscription image generation works again after GPT-5.5 left Codex.

> **Important**
>
> **Close every other Caudra session before you start Preview 10.** On its first start, Preview 10 upgrades the session database in your [state directory](/docs/configuration/#directory-layout) and needs exclusive access, including from editor sessions over ACP and `caudra storage` commands. While any of them has the database open, Preview 10 lists them and does not start, and nothing is changed.
>
> The upgrade keeps your sessions and saves the old database as `caudra.db.v28.bak` next to `caudra.db`. Preview 9 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

> **Warning**
>
> **Before you start Preview 10, rename `ui.tool_output_lines.python_execution` to `ui.tool_output_lines.code`.** A `caudra.toml` that still sets the old key fails to load, with an error that names the new one. Permission rules, `agent.disabled_tools` entries, and system prompt profiles that name the `mcp` or `python_execution` tool no longer match anything, so name `code` in them instead. See [Code](/docs/code/).

> **Warning**
>
> **If you run a remote Workcell server, update it with Caudra and rename its `python_execution` tool group to `code`.** `--tool-group python_execution` and `python_execution` in `WORKCELL_MCP_TOOL_GROUPS` fail at startup, and Caudra refuses a server that does not serve `code`. See [Remote workspaces](/docs/remote-workspaces/).

> **Warning**
>
> **If your workflows call `decide()`, rewrite their questions when you update.** Questions are now a list, and each names itself with `name`. `noul` questions become `predicate` questions, and `criteria` becomes `choices` for a choice and `levels` for a score. Answers come back as `answers`, a list in question order, and `by_name`, keyed by name, and a predicate's probability is in `probability`. A workflow that passes the old shape fails at its first `decide()` call with a catchable error, and so does a run started before the update when you resume it, even at a call it had already answered. See [Typed decisions](/docs/workflows/#typed-decisions).

> **Warning**
>
> **If your config, profiles, or remote Workcell use the code-graph tools, update them together with Caudra.** `code_context` and `code_refs` are now `source_code_context` and `source_code_refs`. `code_map`, `code_impact`, and `code_expand` are gone: use `source_code_context` for `code_map` or `code_expand`, and `source_code_refs` with `depth` for `code_impact`. The profile tool group `code_graph` is now `source_code`, `[decisions.features] code_context` is now `source_code_context`, and an old name in a config or profile is rejected as unknown. A remote Workcell must run the same release as Caudra, with `--tool-group source_code` instead of `--tool-group code_graph`. The SDK's tool kinds are now `SourceCodeContext` and `SourceCodeRefs`. Your sessions, `/tools` usage, decision log, and saved permission rules move to the new names on first start. See [Code Intelligence](/docs/tools/#code-intelligence).

> **Warning**
>
> **Before you start Preview 10, move your decision endpoint out of `caudra.toml`.** The `[decisions]` table now holds only the settings every endpoint shares. `base_url`, `protocol`, `model`, `api_key_env`, `allow_remote`, and `allow_http` in it, or in `caudra.setup({ decisions = ... })`, stop Caudra with an error that shows the table to write instead. To migrate:
>
> 1. Run `caudra auth login --decision`, or move those keys into a `[decisions.<name>]` table in the global `providers.toml`, such as `[decisions.typesafe]`. Then delete them from `[decisions]`.
> 2. Delete `allow_remote`. Configuring an endpoint is now the consent to send decision context to it. Keep `allow_http = true` only on an endpoint that uses plain HTTP to a host other than numeric loopback.
> 3. Delete `decision_engine` from `[experimental]`. It is now an unknown key.
> 4. If you set `TYPESAFE_BASE_URL` or `OPENAI_BASE_URL` to move the decision endpoint, write `base_url = "{env:TYPESAFE_BASE_URL}"` in its table instead. Neither variable changes a decision endpoint any more.
> 5. Run `caudra decisions use <name>` for a table you wrote by hand. Login makes a new endpoint active, but a hand-written one stays inactive until you select it.
>
> See [Decision endpoints](/docs/providers/#decision-endpoints).

> **Important**
>
> **Intel macOS users building with Nix must switch to the native build.** Current nixpkgs no longer supports Intel macOS, so the flake no longer provides `x86_64-darwin` outputs. Native Intel macOS builds and release binaries remain supported.

### Code

- **One `code` tool replaces `mcp` and `python_execution`.** The model runs all of its Python in `code`: computation, MCP server calls, history search, and the decision helpers. It is offered in every session, with or without MCP servers, and its definition never changes, so the prompt cache holds. Python always runs on your machine, also in remote Workcell sessions, with 30 seconds of computation per run where `python_execution` stopped at 5 seconds, and the clock stops while a snippet waits for a call. `timeoutSec` is gone. Variables persist between runs, and `reset: true` starts a run empty. `/tools`, `caudra tools`, and `/mcp` mark MCP tools `via code`, and a session with an `mcp` card from Preview 9 still opens. See [Code](/docs/code/) and [Where snippets run](/docs/code/#where-snippets-run).
- **Search your past conversations.** `history.search()` finds what you wrote, what the model answered and reasoned, and what its tools did, in this conversation, this project, another project, or every stored session, compacted turns and subagents included. `history.read()` opens a hit in full, and `history.sessions()` lists sessions. Searching this project never asks, and another project or all projects ask first. With `[decisions.features] history_search = "enforce"`, the decision engine reranks the matches and returns the 10 most relevant. See [Search past conversations](/docs/code/#search-past-conversations) and [Ranking](/docs/code/#ranking).
- **Decisions and reranking in snippets.** With a decision endpoint active, a snippet can call `decide()` for typed answers about its data, and `rerank()` to keep only the candidates relevant to a query, for up to 256 candidates, so the rest never reach the model. Calls need no approval and work in plan mode. Each shows as a row on the `code` card with a bar per option and a warning when the winner is uncertain, and `/decisions` draws logged answers the same way. See [Decisions and reranking](/docs/code/#decisions-and-reranking).

### Decisions

- **Decision models in `/login` and `/model`.** `/login` has a **Decision models** section with TypeSafe Jev, OpenAI Luna, and a custom endpoint, and a new endpoint becomes the active one. In `/model`, the **Decisions** row of the Jobs section switches endpoints, turns decisions off, or adds one, and `/decision-model` opens it directly. `/decision-next` or `Ctrl+X d` moves to the next endpoint. While an endpoint is active, the status bar shows `[decisions: <name>]`. See [Decision endpoints](/docs/providers/#decision-endpoints).
- **Decision endpoints from the command line.** `caudra auth login --decision` and `caudra auth logout --decision <name>` add and remove an endpoint and its key, and `caudra decisions list`, `use <name|off>`, and `status` show and switch them. Without a saved key, Caudra reads the variable that the endpoint's `api_key_env` names, such as `TYPESAFE_API_KEY` or `OPENAI_API_KEY`. See [CLI](/docs/cli/#caudra-decisions).
- **Auto mode and the decision engine are no longer experimental.** `/auto`, `--auto`, SDK `--permission-mode auto`, `/decisions`, and `caudra decisions` work without a switch, and `always_auto = true` and sessions saved in Auto start in Auto again. With no active endpoint, Auto runs the calls that deterministic checks clear and prompts for the rest. Every decision feature stays off until you turn it on. See [Auto mode](/docs/permissions/#auto-mode).
- **Workflow `decide()` takes OpenAI Decisions questions.** Choice options and score levels can carry a description, and each answer lists the probability of every option or level.
- **`caudra auth login` keeps your comments in `providers.toml`.** Login and logout replace only the table they change.

### Transcript

- **Pictures in the transcript.** `view_image` and `image_generate` cards, Markdown images that point at local files, and prompts with attached images show the picture as a thumbnail, at full resolution in terminals with Kitty graphics and as coloured blocks elsewhere. Click a picture to open it at full size, then press `o` to open it in your system viewer, `w` to open it in the workbench, or `y` to copy its path. Markdown images are never fetched from the network. Set `[ui] inline_images` to `"blocks"`, `"sextants"`, `"kitty"`, or `"off"` to choose. See [Images](/docs/markdown/#images).
- **Search past sessions.** `/search-history` searches this project's saved sessions, and `Ctrl+F` still searches the current chat. `Tab` switches between This chat, Project, and All projects. Results show each match with its session and age and a preview, and words can carry `kind:`, `tool:`, `since:`, and `until:` filters. `Enter` opens the session and scrolls to the match. See [Search past sessions](/docs/sessions/#search-past-sessions).

### Questions and `/btw`

- **Add a note to your answers.** On a question the agent asks, `F5` opens a note for anything the options leave out. It reaches the agent with your answers, or with the dismissal when you press `Esc`. The form's hints now start with `Enter submit`. See [Keybindings](/docs/keybindings/#form).
- **Drag to copy in `/btw` and the question form.** Dragging over a `/btw` thread copies the passage when you let go, the way the transcript copies. In the question form, a drag copies instead of picking the option it ends on, and `Ctrl+C` copies a selection before it stops the agent. See [Commands](/docs/commands/).

### Goals

- **A goal that stops getting anywhere stops its run.** When three turns in a row make no progress, the run ends with the goal still active instead of spending continuations until the limit. Autopilot reviews the stall like a loop cap. Headless clients get a `stalled` goal event, and a `goal_finished` automation trigger that lists `stalled` in `verdicts` fires too. See [Commands](/docs/commands/) and [Automations](/docs/automations/).
- **A goal waiting on something outside the session waits instead of stalling.** When the goal waits on a change such as a CI run or a deploy, the run ends and the goal is checked again after a minute, then twice as long each time up to 30 minutes, for about two hours before it stalls. A message from you ends the wait, and a resumed session keeps it. Headless clients get a `waiting` goal event.

### Tools

- **Two source code tools instead of five, under new names.** `source_code_context` (formerly `code_context`) finds the code a task touches, and `source_code_refs` lists who calls a symbol and from which lines. With `depth`, `source_code_refs` shows what a change could reach and how many of those symbols are tests, which is what `code_impact` did. Both tools are always in the request for every model.
- **More precise source code results.** Standard library calls such as `.clone()` or `Vec::new()` no longer link to a symbol of the same name in your tree, and `source_code_context` lifts only names written as code, so an ordinary word such as "renew" no longer pulls in every `new`.
- **`/tools` counts calls made inside `batch`.** Each call a batch makes has its own row.

### Fixes

- **ChatGPT subscription image generation uses a supported model.** Image requests now run through GPT-6.1 Sol instead of GPT-5.5, which OpenAI retired from Codex with ChatGPT sign-in on October 14, without changing your configured image model or switching to API billing.
- **Lua job cleanup.** Releasing an abandoned `jobwait` no longer aborts the process.
- **Shell output filters.** Bundled filters no longer match unrelated subcommands that start with a filtered name.
- **File indexes and code graphs.** PHP clone arguments and partial application no longer report parse errors, and Go type aliases are included.
- **SVG source icons.** An icon over the pixel budget is skipped, including through nested masks, clips, and filters.
- **HTML conversion.** Code blocks keep blank lines and lazy-loaded images resolve.
- **Nix builds.** Evaluation works without building the worker source first.

## Caudra 0.2 Preview 9 {#v0-2-0-preview-9}

[v0.2.0-preview.9 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.9)

- Code mode: the model calls your MCP servers from Python, so a large server costs a short list, results are filtered before the model reads them, and the prompt cache survives.
- Gemini 4 Argon is the Google default, with new Gemini Flash models, Grok 4.7, GLM-5.3, DeepSeek V4.1 Flash, and Mistral Large 4. Retired models are gone, and Google requests work again.
- The agent can ask questions without waiting, and you can answer later.
- Autopilot runs without a continuation limit, answers the agent's questions, and logs what it did.
- A log of every goal check, and goal checks survive an interrupted reply.
- `/worktree close` brings a worktree's work back to the main checkout.
- `/model` opens on your favorites, and `Home` and `End` reach the ends of a list.
- macOS updates no longer stall, and updates use your GitHub token and show download progress.

> **Important**
>
> **Close every other Caudra session before you start Preview 9.** On its first start, Preview 9 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 9 does not start. It lists the open sessions to close instead, and nothing is changed.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `caudra.db.v25.bak` next to `caudra.db`. Preview 8 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

> **Warning**
>
> **Before you start Preview 9, delete `defer_tools` and `always_load` from `mcp.toml`.** Code mode replaces the tool search they tuned. A global or project `mcp.toml` that still sets either fails to load: Caudra starts without that file's servers, and `/mcp` shows an error that names the key.

### MCP

- **Code mode.** The model now reaches your MCP servers by writing Python in one `mcp` tool, instead of receiving a definition for every tool. A snippet can call several tools at once and filter or join their results before the model reads anything, so a list of 500 issues can come back as three lines. The model learns your servers from a short list in the conversation, and `search()` finds the tools the list leaves out. The tool definitions stay the same while a server connects, fails, or changes its tools, and when you switch to plan mode, so none of these makes the provider read the whole conversation again at full price. See [Code mode](/docs/mcp/#code-mode) and [The prompt cache](/docs/configuration/#the-prompt-cache).
- **Approvals work as before.** Each tool call in a snippet asks on its own, and permission rules, `agent.disabled_tools`, and profiles still name a tool `server.tool`. Plan mode and read-only agents still allow only the read-only tools of recognised servers. Snippets run in a separate worker process with no filesystem, network, subprocess, or environment access. See [MCP tool calls](/docs/permissions/#mcp-tool-calls) and [Where snippets run](/docs/mcp/#where-snippets-run).
- **Snippets are checked before they run.** A misspelt tool, an argument of the wrong type, or code that is not valid Python stops a snippet before any server call, naming the closest tool and the line. Slips with one meaning, such as a missing `import asyncio` or a Markdown fence around the snippet, are fixed and listed on the card. See [Results and errors](/docs/mcp/#results-and-errors).
- **`mcp_resource` is gone.** The model reads a server's resources with `resources()` and `read_resource()` in `mcp`, and reads still ask first. See [Resources](/docs/mcp/#resources).
- **Servers with a code mode of their own.** Add `?codemode=false` to the URL of Cloudflare's MCP server, or `?mode=passthrough` to Executor's, so the model calls their plain tools. See [Servers with a code mode of their own](/docs/mcp/#servers-with-a-code-mode-of-their-own).
- **`/mcp` and `/tools`.** `/mcp` shows the name the model calls each server by, and warns at startup when no code worker can run snippets. `/tools` counts each server call a snippet makes as a row named `server.tool`. See [What the tools cost](/docs/token-economy/#what-the-tools-cost).
- **Smaller changes.** Sessions from Preview 8 and earlier resume as before. File revert records an `mcp` run only once it calls a tool that may change files. Python now runs on Monty 1.1.0, which supports complex numbers. See [Recording](/docs/sessions/#recording).

### Models

- **Gemini 4 Argon.** `google/gemini-4-argon` is the Google Best model and the model Caudra picks when you use Google without choosing one, in place of `gemini-2.5-pro`. It costs $2 per million input tokens and $10 per million output tokens, with cache reads at $0.10, until November 30, 2026, then $4 and $20 from December 1, 2026. Caudra switches to the new price on that date without an update. Argon always thinks, so thinking `off` runs at `low`. It is not on Google's free tier, so it needs a key on a project with billing enabled. On the free tier, pick `google/gemini-3.8-flash`. When a key has no quota for a model, Caudra now says so at once instead of retrying. Caudra uses an API key for Google, since Google's terms forbid third-party tools from using the sign-in of Gemini CLI or Antigravity. See [Google](/docs/providers/#google).
- **Gemini 3.8 Flash, 3.6 Flash, and 3.5 Flash-Lite.** 3.8 Flash and 3.6 Flash cost $0.75 per million input tokens and $3.75 per million output tokens until December 31, 2026, and $1.50 and $7.50 from January 1, 2027. 3.5 Flash-Lite is the Google Fast model, in place of `gemini-2.0-flash-lite`, which Google shut down. Gemini 2.5 Pro and 2.5 Flash leave the model table, since Google no longer serves them to new keys.
- **Google models answer again.** Every request to a Google model failed with `Invalid JSON payload received. Unknown name "$schema"`. Caudra now sends tool definitions in a form Google accepts. Switching to Gemini 3 or 4 in the middle of a turn no longer makes Google refuse the request.
- **Grok 4.7.** `xai/grok-4.7` is the xAI default, in place of `grok-4.6`. It costs $2 per million input tokens and $6 per million output tokens, with cache reads at $0.50. Once a prompt reaches 200,000 tokens, every rate doubles for the whole request, as xAI bills it.
- **GLM-5.3 on Z.AI.** `zai/glm-5.3` is the Z.AI Best model and the default on the pay-as-you-go plan, in place of `glm-5.1`. It costs $1.40 per million input tokens and $4.40 per million output tokens, takes a 1M-token context, and always thinks. The Coding plan keeps `glm-5-code`. `glm-5.3-flash` and `glm-5.3-flashx` join as small models that read images. Caudra now bills `glm-5.2` and `glm-5.1` at Z.AI's current $1.40 and $4.40, up from $1 and $3.20.
- **DeepSeek V4.1 Flash.** `deepseek/deepseek-flash` is the DeepSeek default, in place of the retired `deepseek-v4-flash`. It reads images and costs $0.15 per million input tokens and $0.60 per million output tokens off-peak, twice that at peak hours.
- **Mistral Large 4 and GLM 5.3 on Mistral.** `mistral/mistral-large-4`, in public preview, costs $0.68 per million input tokens and $2.09 per million output tokens at its launch price, and $1.36 and $4.18 from October 20, 2026. `mistral/glm-5-3` replaces `glm-5-2` at the same price. Mistral retires GLM 5.2 on October 31, 2026.
- **Claude Sonnet 4.6 answers at full length.** `anthropic/claude-sonnet-4-6` can now write up to 128,000 output tokens, up from 64,000.
- **Retired models leave the model table.** Caudra drops `claude-sonnet-4`, `claude-opus-4-0`, and `claude-opus-4-1`, which Anthropic retired. Use `claude-sonnet-5-5` or `claude-opus-4-8` instead. It also drops `gpt-5.1-codex`, `gpt-5.1-codex-max`, `gpt-5.1-codex-mini`, and `gpt-5.2-codex`, which OpenAI shut down. Use `openai/gpt-5.6-sol`, or `gpt-5.6-terra` in place of `gpt-5.1-codex-mini`.
- **Copilot follows GitHub's model list.** Copilot and Copilot Enterprise add `gemini-3.8-flash`, `gemini-4-argon`, and `grok-4.7`, and drop the models GitHub no longer serves, such as `gemini-3.5-flash`, `kimi-k2.7-code`, and `claude-opus-4.7`. `claude-sonnet-4.6` stays, because GitHub still serves it on annual Copilot Pro and Pro+ plans.

### Questions

- **Questions that do not stop the agent.** The agent can ask a question it does not need answered for its next steps, such as a preference or a name, and keep working. Press `Esc` to put the form aside in a `[? N]` chip in the footer, and `Ctrl+X u` or a click on the chip brings it back. Your answer reaches the agent when you give it, even after its turn has ended. On a question the agent waits on, `F4` puts it off and lets the agent carry on. Turn this off with `deferred_questions = false` under `[agent]`. See [Deferred questions](/docs/sessions/#deferred-questions).

### Autopilot

- **No continuation limit.** `max_continuations = 0` under `[autopilot]` lets an engagement run until its goal ends, its cost or time limit runs out, or you stop it. See [Budgets](/docs/autopilot/#budgets).
- **An audit log of what autopilot did.** The status panel gains a Log tab, which `/autopilot log` opens at any time. It lists every decision of the session by engagement, with what the delegate was asked, what autopilot said and why, the model and cost, and what happened next. Press `f` to filter, `y` to copy an entry, and Enter to jump to its row in the transcript. When you answer a decision differently, the delegate sees your answer in its next decisions. SDK clients read the log with the new `autopilot_log` control. See [The log](/docs/autopilot/#the-log).
- **Autopilot answers the agent's questions.** While engaged, autopilot now answers the main agent's `question` forms and a server's forms during its tool calls, as Preview 8 described.

### Goals

- **A log of every goal check.** The `/goal` panel gains a Log tab, which `/goal log` opens. It lists every goal of the session and every check of it, with its status, time, evaluator model, cost, and reason. The same keys filter, copy, and jump to the transcript, and a goal's check rows now come back after a resume. SDK clients read the log with the new `goal_log` control. See [The goal log](/docs/commands/#the-goal-log).
- **Goal checks survive an interrupted reply.** After you interrupted the agent while it was thinking, every later goal check on a Claude model failed with `thinking.signature: Field required`. Checks now leave out the agent's reasoning, and a goal whose checks were failing is checked normally again.

### Worktrees

- **`/worktree close` brings the work back.** Closing a worktree is the reverse of creating one. It fast-forwards the main checkout's branch to the worktree's commit, moves the worktree's sessions back, carries its uncommitted changes into the main checkout, and removes it, keeping the branch. The removal confirmation offers it first, or says why it cannot, such as when the main checkout has commits of its own. Removing and closing now check for a pending revert or an active workflow before they change anything. Leaving a worktree's own Herdr workspace or Orca terminal brings every open session back, each in its own tab. See [Closing a worktree](/docs/worktrees/#closing-a-worktree) and [Removing a worktree](/docs/worktrees/#removing-a-worktree).

### Pickers

- **`/model` opens on your favorites.** With favorite models, `/model` and `Ctrl+X m` put the cursor on the current model's favorite row, or on the first favorite, instead of on the Chat job. See [Model jobs](/docs/providers/#model-jobs).
- **`Home` and `End` reach the ends of a list.** In every picker, `Home` and `End` jump to the first and last row while the filter is empty. In the workflow and automation inspectors, they and `PageUp` and `PageDown` move the list that has the focus. See [Pickers](/docs/keybindings/#pickers).

### Usage

- **Spend by purpose for the session.** `/usage` now shows a Per purpose table for the current session, counted for turns from Preview 9 on. In the lifetime view, click `... and N more` to show the rest of a list, or press `e` to open every list at once. See [Commands](/docs/commands/) and [Lifetime spend](/docs/token-economy/#lifetime-spend).

### Sessions

- **Tasks run on the favorite you name.** When you ask for a model by a short name, as in "ask sonnet to review this", the task runs on your favorite model that matches it. The main agent is also told your favorites when it can choose a task's model. See [Models and reasoning](/docs/commands/#models-and-reasoning).
- **Forks keep their permission mode.** A fork now inherits the parent's Ask, Auto, or YOLO mode, also in another tab or a new worktree.
- **Mode and model switches apply once the session settles.** A `Tab` toggle now takes effect as soon as nothing runs, instead of waiting for your next message. See [Selecting Plan while work runs](/docs/queue/#selecting-plan-while-work-runs).
- **`/btw` answers when the model reaches for a tool.** Instead of stopping, Caudra tells the model that tools are unavailable, so it answers from the conversation. See [Commands](/docs/commands/).

### Updates

- **Updates on macOS no longer stall.** Updating a Mac that still had the large license bundle from Preview 1 stayed on `downloading caudra …` for about four minutes. It now takes under a second.
- **Updates use your GitHub token.** `caudra update` and the update notice now send `GITHUB_TOKEN` or `GH_TOKEN` to GitHub, as the installer does, so a shared IP no longer fails with `server returned HTTP 403`.
- **Download progress.** The shell installer shows a progress bar for the release archive. From Preview 9 on, `caudra update` downloads the archive itself, shows its size, speed, and time left, and checks it against the release checksums.

## Caudra 0.2 Preview 8 {#v0-2-0-preview-8}

[v0.2.0-preview.8 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.8)

- Claude Haiku 5.5 is the Anthropic Fast model.
- Caudra speaks the current MCP protocol, and servers can ask you questions, report progress, and share resources.
- The experimental decision engine reranks memory notes, tool search, retained tool output, and code context, the transcript shows what each decision changed, and workflows gain `rerank()`.
- Favorite and 10 recent models in `/model`, and tasks run on the model you name.
- Forks open in a new Herdr or Orca tab, and tabs are named after the session.
- New sessions can open in build mode, and `/sessions` can delete the current session.
- Fixes for list scrolling, thinking levels, session moves, Orca's bell, and `caudra update` output.

> **Important**
>
> **Close every other Caudra session before you start Preview 8.** On its first start, Preview 8 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them has the database open, Preview 8 lists the sessions to close and does not start, and nothing is changed.
>
> The upgrade keeps your sessions and saves the old database as `caudra.db.v24.bak` next to `caudra.db`. Preview 7 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

> **Note**
>
> **If you raised `decisions.timeout_ms` for a slow decision engine and use `tool_search`, set `rerank_timeout_ms` as well.** Tool search now uses that deadline, which defaults to 2 seconds.

### Models

- **Claude Haiku 5.5 is the Anthropic Fast model.** `anthropic/claude-haiku-5-5` costs $0.10 per million input tokens and $0.50 per million output tokens, with cache reads at $0.01. Once a prompt passes 100k tokens, cached tokens included, the whole request costs five times those rates. It runs at a 372k working window, and `claude-haiku-5-5-1m` opens the full 1M window. Copilot lists it as `claude-haiku-5.5`. It replaces `claude-haiku-4-5` as the Fast model, so on an Anthropic session, session titles, completion-goal checks, and other Fast work run on it unless you bound those jobs to a model. Bind Fast to `anthropic/claude-haiku-4-5` in `/model` to keep the older model. See [Anthropic](/docs/providers/#anthropic) and [Model jobs](/docs/providers/#model-jobs).
- **Sonnet 5.5 cache reads cost less.** Caudra now counts Claude Sonnet 5.5 cache reads at the new price of $0.10 per million tokens, on Anthropic and Copilot.
- **Thinking levels a model rejects are no longer sent.** `minimal` now sends the shallowest level a model has, where it used to send `none` and turn reasoning off. On Copilot, thinking `off` on a model that always reasons, such as `gpt-6.1-sol`, runs at its lowest level instead of failing with `Unsupported value: 'none' is not supported`. A `none` setting now means the same as `off` on every model. A custom `openai` or `openai-responses` model without `reasoning_options` takes no thinking setting. See [Model defaults](/docs/providers/#model-defaults).

### Model picker and tasks

- **Favorite models.** In `/model`, press `Ctrl+F` on a model to mark or unmark it as a favorite. Favorites are listed first and kept across sessions, and `/favorite-next` or `Ctrl+X c` switches to your next favorite. Recent now keeps the last 10 models instead of 4, without your favorites. Every model also keeps its row in its provider's section, marked `★` for a favorite and `↺` for a recent model. See [Model jobs](/docs/providers/#model-jobs).
- **Run a task on a model you name.** Ask for a model, as in "ask opus and sol to review this change", and the main agent starts one task per model you name. The `task` tool takes a `model`, either an exact `provider/model` name or a short name such as `opus` that resolves to the newest match, and a `thinking` level from `off` to `max`. Both win over a profile's `subagent_model` and `subagent_thinking`. A name that fits several models fails and lists them. A task's result, footer, and card name the model and level it actually runs on. `[agent] task_model_choice = false` turns this off. See [Models and reasoning](/docs/commands/#models-and-reasoning).

### Decision engine

- **Reranking features.** Six new experimental decision features use the engine as a reranker, and each is off by default. `memory_search` lists relevant memory matches first and names the ones it left out. `memory_recall` reminds the agent of up to three notes that may matter for your new message. `memory_duplicates` points out an existing note a new one may repeat. `compaction_retention` keeps the tool results that matter to the current goal when a session compacts, instead of only the newest three. `output_pointers` points a truncated tool result at the omitted lines most likely to matter. `code_context` moves the most relevant rows of a `code_context` result to the top. When the engine fails or times out, each behaves as if it were off. They use a new `rerank_timeout_ms` deadline (2 seconds) and `thresholds.relevance_floor` (`0.1`). With `tool_search` enforced, tool search now scores and reorders its first 16 keyword matches instead of picking one, and never drops a match. See [Reranking](/docs/configuration/#reranking).
- **Workflows can rerank.** `rerank(query, candidates)` returns `#{ index, score }` entries, most relevant first, with the options `model`, `timeout_ms`, `threshold`, and `top_k`. Each batch of 16 candidates is one journaled decision, so a resumed run asks only for the rest. In `/workflow`, `decide()` and `rerank()` calls are labelled with their question IDs or candidate counts. A `decide()` or `rerank()` timeout may now reach the larger of `timeout_ms` and `rerank_timeout_ms`. See [Reranking candidates](/docs/workflows/#reranking-candidates).
- **Decision rows in the transcript.** When a decision feature changes what happens, the transcript shows a row that names the feature and what it did, such as `code_context · moved 3 of 12 rows up`. A row also appears when the engine fails and the default applies, and a dim `shadow` row shows what a feature in shadow mode would have done. Click a row to see the candidates and scores behind it. Rows are saved with the session, the model never sees them, and `[ui] show_decisions = false` hides them. See [Decision rows](/docs/permissions/#decision-rows-in-the-transcript).
- **Shell duration estimates read plainly.** A shell card now shows an estimate only for a command expected to take more than a second, in the units of its timeout, such as `ETA 10s–2m`.

### MCP

- **The current MCP protocol.** Caudra now speaks MCP `2026-07-28` and still speaks the older protocol that starts with `initialize`, so servers built on either work. When a server announces that its tools, prompts, or resources changed, Caudra lists them again. Caudra never offers roots, sampling, or logging, and writes server log messages to `caudra.log` at debug level. See [Protocol versions](/docs/mcp/#protocol-versions).
- **Servers can ask you questions.** During a tool call, a server can ask for input with a form of typed fields or a link to open, such as a sign-in page. The question form checks each answer and lets you review before sending. `F3` declines and `Esc` cancels. A link opens in your browser only after you agree. Autopilot may answer a form from the main agent's call, but never a link, a form from Executor, or a form asking for a credential. ACP clients that support MCP elicitation get the question too. See [Server questions](/docs/mcp/#server-questions).
- **Long calls report progress.** A server's progress shows beside the card's label, and each report restarts the `timeout` wait, up to ten times `timeout`. Stopping the run tells the server the call was cancelled. After a dropped connection, a call that is not safe to repeat fails with `connection lost; the call's outcome is unknown`. See [Long calls](/docs/mcp/#long-calls).
- **Resources and completions.** Mention a server's resource as `@server:uri`, and Caudra reads it into your message like a mentioned file. Typing `@` lists resources beside your files, and template values and prompt arguments complete from the server. The model can list and read resources with the new `mcp_resource` tool, which works in plan mode and asks first. A prompt's images and embedded resources now reach the model. See [Resources](/docs/mcp/#resources).
- **Server instructions reach the model.** The instructions each running server gives for its tools are added to the system prompt, up to 4096 bytes per server. See [Server instructions](/docs/mcp/#server-instructions).
- **Executor and Home Assistant are recognised.** Executor gets a card that shows how a run ended, its logs, and what a paused run waits for. An Executor `resume` call that approves a paused run always asks you, even under autopilot. Add `?elicitation_mode=native` to Executor's URL so it asks you directly. Home Assistant gets cards for intents and its live context, and a call that controls a `lock`, `alarm_control_panel`, `cover`, or `valve` always asks, even with an allow rule or in Auto. See [Recognised servers](/docs/mcp/#recognised-servers).
- **Read-only tools work in plan mode.** Tools that Executor or Home Assistant mark read-only, such as Home Assistant's `GetLiveContext`, can be called in plan mode and by read-only agents. They still ask, with an option to always allow the tool. See [MCP tool calls](/docs/permissions/#mcp-tool-calls).
- **MCP cards open whole.** A finished MCP call's card shows its arguments and its whole result instead of the first three rows. Compact view and `ui.always_collapsed` still fold it. A card that ends with a `click for raw` row switches to the result's raw JSON and text. A result the server marks as an error draws as a failed card. See [Results in the transcript](/docs/mcp/#results-in-the-transcript).
- **`/mcp` shows what a server reported.** Select a running server to see its name and version, the protocol version, the features it offers, and the first lines of its instructions. See [Runtime toggling](/docs/mcp/#runtime-toggling).
- **A sign-in the server refuses is reported.** `caudra mcp auth` now checks that the server accepts the stored sign-in, and signs in again when it does not. When a fresh sign-in is still refused, as on Home Assistant's `/api/mcp` for a user who is not an administrator, `/mcp` shows the server as Refused instead of asking you to sign in again. Home Assistant's `/api/mcp/assist` works for any user. See [OAuth](/docs/mcp/#oauth).

### Interface fixes

- **The wheel scrolls lists.** In `/decisions`, `/automations`, `/workflow`, `/peers` and `/messages`, the wheel over the list now scrolls it instead of moving the selection.
- **Files picked with `Ctrl+S` are mentioned.** The file picker now inserts the path as an `@` mention, so the file is attached when you send the message. See [Mention a file with @](/docs/context/#mention-a-file-with).
- **The `Ctrl+X` panel takes the mouse.** Click a chord in the panel to run it, or click anywhere else to cancel.
- **Hidden Orca tabs ring when a turn ends.** A hidden Caudra tab in an Orca remote workspace could show `working` long after its turn ended. Caudra now rings the bell when a turn ends or a prompt waits while its terminal does not have focus, so Orca marks the tab unread, and sends its status again when you return. Turn on **Terminal Bell** in Orca's notification settings for a desktop notice as well. `notifications = "off"` stops the bell. See [Orca](/docs/notifications/#orca).
- **Prompts, popups and scrollbars follow the theme.** The permission prompt, the `@` and `#` completion lists, the sandbox manager, and scrollbars now use the theme's colours, so a dark theme on a light terminal no longer shows light patches.

### Sessions

- **Open new sessions in build mode.** Set `default_mode = "build"` under `[ui]` to start new terminal UI sessions in build mode instead of plan mode. A resumed session still reopens in the mode it was left in. See [`ui`](/docs/configuration/#ui).
- **Delete the current session.** In `/sessions`, `Ctrl+D` on the current session now deletes it after the usual confirmation and opens a new session in its place, in the same mode and directory. A session that is still working cannot be deleted until it is idle. See [Sessions](/docs/commands/#sessions).
- **Tabs are named after the session.** Caudra sets the terminal window title to the focused session's title, so most terminals, Orca, and Herdr label the tab with it. A tab you named yourself keeps its name. See [Tab names](/docs/sessions/#tab-names).
- **Forks open in a new Herdr or Orca tab.** Inside a Herdr pane or an Orca terminal, **Fork here** resumes the new session in a new tab with a Caudra of its own, and the session you forked from stays open. Remote, sandbox, and ephemeral sessions still fork into a tab of the current Caudra. See [Forks in Herdr and Orca](/docs/sessions/#forks-in-herdr-and-orca).
- **Memory writes wait out a busy database.** The `memory` tool failed with `database is locked` while another Caudra process held the database for more than 5 seconds. Memory writes now retry for up to 30 seconds, like session saves do.
- **Pending reverts no longer block moves.** `/move-session`, `/migrate-sessions`, and `/worktree remove` were refused or failed to start when a session already in the destination had a pending revert. Sessions in the destination no longer affect a move.

### Command line

- **`caudra update` no longer prints its installer.** It names the release it moves to, installs it without asking, and ends with the `caudra rollback` command that returns to your current version. `caudra update --show-script` prints the checksum-verified installer and asks before running it, as before. `-y` and `--no-color` are still accepted, so existing scripts keep working. See [`caudra update`](/docs/cli/#caudra-update-caudra-rollback).

## Caudra 0.2 Preview 7 {#v0-2-0-preview-7}

[v0.2.0-preview.7 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.7)

- Context windows match the limits providers enforce, and can be set for built-in providers.
- Global config files read secrets from files.
- GPT reasoning on Copilot stays in one piece.
- Copying in WSL reaches the Windows clipboard.
- Only the plan menu approves a saved plan.
- A session saves again after another Caudra carried it on.

> **Important**
>
> **Keep `{file:PATH}` out of your config files until every Caudra process that reads them runs Preview 7.** Older versions read it as literal text.

### Providers

- **Copilot reasoning stays whole.** GPT models on Copilot and Copilot Enterprise split their reasoning into a block per streamed word, sent it back to the model as plain reply text, and dropped the encrypted reasoning later turns need. Each reasoning summary is now one block, and later turns send the encrypted reasoning back as they do with OpenAI. Turns saved before the update keep their split blocks.
- **Context windows match the limits providers enforce.** Copilot models now use Copilot's prompt limit as the window, so `copilot-enterprise/gpt-6.1-sol` runs at 272k instead of 400k and compacts before Copilot refuses the prompt. OpenCode models with a separate input limit, such as the GPT-5 models, now run at their full input limit instead of compacting early. On any model whose reply shares the window, compaction now leaves room for the reply, up to half the window, so a model like `claude-haiku-4-5`, with 64k of a 200k window, is no longer refused near the limit. `/context` and the status bar show the new point.
- **Set a built-in model's window in `providers.toml`.** A `[[SLUG.models]]` entry or a `[SLUG.model_defaults]` table now sets `context_window` and `max_output_tokens` for a built-in provider such as `anthropic`, ahead of what the provider lists. Caudra warns about any other model key there, since built-in providers ignore it. See [Model limits on a built-in provider](/docs/providers/#model-limits-on-a-built-in-provider).

### Configuration

- **Values from files.** Beside `{env:NAME}`, a string value in the global `caudra.toml`, `permissions.toml`, `mcp.toml`, `providers.toml`, or `workcell.toml` can now hold `{file:PATH}`. Caudra puts the file's text there, trimmed at both ends, so a secret kept in a file stays out of the config. A relative path starts at the config file's directory, and a path can hold `{env:NAME}`, such as `{file:{env:CREDENTIALS_DIRECTORY}/token}`. The file must be a UTF-8 regular file of at most 1 MiB. Project files under `.caudra/` cannot use references. See [Values from files](/docs/configuration/#values-from-files).

### Terminal

- **Copying works in WSL.** Copied text often stayed in the WSLg clipboard and never reached Windows. Every copy in WSL now also goes through OSC 52, which Windows Terminal and Orca pass to the Windows clipboard. In Orca, this needs Allow TUI Clipboard Writes (OSC 52), which older Orca releases leave off.

### Plan mode

- **A saved plan goes to the plan menu.** With the question-tool nudge on, the model could ask whether to implement a saved plan, and that answer could not leave plan mode. Once the plan is saved, the nudge now stays off until your next message and the model ends with a summary. Approve the plan from the plan menu as before.

### Sessions

- **Reopening a session another Caudra carried on saves again.** If you left a session with `Ctrl+X n` or `/new`, carried it on in another Caudra, and reopened it in the first, every save failed with "session … was modified concurrently", and no other Caudra could open it until you quit. Deleting it from the session picker failed the same way. Both now work. The session on disk was never damaged.

## Caudra 0.2 Preview 6 {#v0-2-0-preview-6}

[v0.2.0-preview.6 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.6)

- Autopilot (experimental) answers permission prompts, questions, and ready plans while a session works toward its goal.
- Chosen environment variables reach shell commands.
- MCP results are drawn readably, and MCP servers without client registration can sign in.
- What is new shows after an update.
- Orca terminal support.
- Enter answers a permission prompt in terminals where it did not.

> **Important**
>
> **Close every other Caudra session before you start Preview 6.** On its first start, Preview 6 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 6 does not start. It lists the open sessions to close instead, and nothing is changed.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `caudra.db.v23.bak` next to `caudra.db`. Preview 5 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

### Autopilot (experimental)

- **Keep a session moving.** With `experimental.autopilot` on, `/autopilot <goal>` sets a goal and hands what would wait for you to a separate delegate model: permission prompts, the `question` tool, questions asked in prose, a plan that is ready, and the stalls that end a run, such as a goal loop cap or a provider outage. Autopilot ends when the goal is met or impossible, a budget runs out, or you stop it. Decisions use the new Autopilot model job, which follows Best unless you bind it in `/model` or with `/autopilot model`. See [Autopilot](/docs/autopilot/).
- **Limits.** The delegate approves a permission prompt once and never adds a rule. It never approves prompts that plan mode raises, tools that always prompt, protected files and commands, or any prompt in a session that content screening flagged. Each engagement may start 100 turns without you by default, and `max_cost` and `max_minutes` under `[autopilot]` add spend and time limits.
- **You stay in charge.** Every decision is a transcript row, and a footer chip opens a panel with the budget and recent decisions. A key or click on a card autopilot is deciding makes it yours. Esc Esc, Ctrl-C while the agent works, or `/autopilot off` stops autopilot. An item it leaves to you raises an urgent notification.
- **Headless and CI.** `caudra -p --autopilot "/goal <condition>"` runs unattended, and a prompt autopilot leaves to a human is denied. SDK clients engage it with `--autopilot` or the `autopilot_start` control. See [Headless Mode](/docs/headless/#autopilot).

### Shell environment

- **Forward the variables your commands need.** `/env` lists every variable Caudra can see, shows which ones reach shell commands, and edits `[agent.shell_env]` in the global `caudra.toml`. `pass` forwards names such as `DOCKER_HOST` or `AWS_*`, and `set` gives commands their own values, with `{env:NAME}` for secrets. A project `.env` never supplies a forwarded value, and a project config cannot hold the table. See [Forwarding more variables](/docs/cli/#forwarding-more-variables).
- **Sandboxes forward their guest's variables.** A sandbox profile's `shell_env` chooses which guest variables reach commands, all of them by default. Values stay in the guest. See [Shell environment](/docs/sandboxes/#shell-environment).

### MCP tool cards

- **Readable results.** Each block of an MCP result is drawn by its type: JSON is indented and coloured, other text is rendered as markdown, and images, audio, and resources become labelled rows with their type and size. The model receives the same text as before.
- **Cleaner header.** A card reads `server › tool` with short arguments beside it, followed by the result's size and an estimate of the tokens the model reads, such as `JSON · 128 lines · ~3.2k tokens`.
- **Code it ran.** When a call runs code, such as a `code`, `script`, or `sql` argument, the card shows it numbered and highlighted above the result. See [Results in the transcript](/docs/mcp/#results-in-the-transcript).

### MCP OAuth

- **Servers without client registration.** Caudra now logs in to servers that accept OAuth client ID metadata documents, such as Home Assistant, with no client to configure. These logins used to fail with "no stored client and server has no registration endpoint". The server must be able to reach caudra.ai, where Caudra's document lives. See [OAuth](/docs/mcp/#oauth).
- **Stricter discovery.** Login discovery follows the MCP spec more closely, and servers without S256 PKCE are refused. A client is reused only with the authorization server that issued it, so the next interactive login after this upgrade gets a new client once. Stored tokens keep refreshing.

### Permission prompts

- **Enter answers a fresh prompt.** In Herdr and some other terminals, Enter did nothing on a new permission prompt until you clicked or pressed another key. A key you were already holding when the prompt appeared is still ignored, and the footer says whether to let go or to wait. See [Permission prompts](/docs/permissions/#permission-prompts).
- **Tooltips.** Resting the mouse on any part of a permission prompt explains what it does and which key presses it.

### What's new and release notes

- **What's new after an update.** The first time the terminal UI starts after an update, it shows the notes for every release since the one you last ran. `/changelog` shows them again, and `/changelog all` shows every release. The notes ship inside Caudra, so they work offline. Set `ui.changelog_on_update = false` to show them only through `/changelog`. Every release's notes are also on the [Release notes](/docs/release-notes/) page and in `/docs`.

### Terminal integration

- **Orca terminals.** Inside an [Orca](https://github.com/stablyai/orca) terminal, Caudra reports to Orca its state, its model, and the label of a prompt that waits for you. `/worktree` creates and opens worktrees through Orca for repositories Orca knows, and git otherwise. Shell commands get Orca's terminal variables, and an `orca` skill teaches the model Orca's CLI. See [Orca notifications](/docs/notifications/#orca) and [Orca worktrees](/docs/worktrees/#orca-integration).
- **More accurate pane status.** Herdr and OSC 7501 pane status now match what Caudra is doing more closely, and automation `notify()` messages always arrive as notifications, even inside Herdr and Orca.

### Sessions

- **To-do list of the task you view.** The sticky to-do list follows the conversation you view, so a task shows its own list or none.

## Caudra 0.2 Preview 5 {#v0-2-0-preview-5}

[v0.2.0-preview.5 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.5)

- Configuration edits apply while Caudra runs.
- Global config files can read environment variables.
- Fewer questions about shell lines that set variables.

> **Important**
>
> **After you update, restart running Caudra sessions** so they pick up hot reload. Until every Caudra process that reads your config files runs Preview 5, keep `{env:NAME}` out of them: older versions read it as literal text. Preview 5 does not upgrade the session database.

### Configuration

- **Edits apply while Caudra runs.** The terminal UI watches its config files, skills, custom commands, and system prompt profiles, and applies an edit shortly after you save it. Permissions apply before the next tool call, MCP servers start, stop, and restart at once, and `[agent]` settings apply from the next turn. Some changes still need `/reload` or a restart, and a footer line says which. Headless runs and ACP still read configuration once, at startup. See [Hot reload](/docs/configuration/#hot-reload).
- **Edits keep the prompt cache.** Caudra keeps the tools and system prompt the model was shown until the cache is lost anyway, such as at a compaction or a model switch, and tells the model about the change in a short reminder meanwhile. Permissions and which tools can run follow your edit at once.
- **Broken files fail safely, without dialogs.** A config file that fails to load keeps its last valid version and shows a red chip in the status bar until it loads again. A broken `permissions.toml` denies every tool call until it loads again. Project permissions and MCP servers that need review show a chip that opens the review.
- **Environment variables in global config files.** A string value in the global `caudra.toml`, `permissions.toml`, `mcp.toml`, `providers.toml`, or `workcell.toml` can contain `{env:NAME}`. Caudra fills it in from the process environment, or else from the global `.env`, so secrets can stay out of the file. An unset variable is an error that names the file, line, and variable. Project files under `.caudra/` cannot use references, and `caudra auth login` and `/mcp` keep a reference in the file when they save it. See [Environment variables](/docs/configuration/#environment-variables).

### Permissions

- **Shell lines that set variables.** `export`, `unset`, `declare`, `typeset`, `readonly`, `local`, and `set` no longer make a whole line unreadable, which allowed only "run once" or "no". `unset FOO; set -euo pipefail; cargo test` now asks only about `cargo test`, with its usual scopes. Setting a variable or clearing one Bash itself reads, such as `PATH`, still counts as a command built at run time, and printing variables is treated like `printenv`.

## Caudra 0.2 Preview 4 {#v0-2-0-preview-4}

[v0.2.0-preview.4 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.4)

- Hover tooltips.
- The workbench explorer shows Caudra's own files and folders you choose.
- Narrower permission scopes for commands that take paths.
- xAI sign-in works again.

> **Important**
>
> **After you update, restart every running Caudra session**, including sessions in other projects. Preview 3 cannot read saved permissions once one of the new folder scopes is remembered. The session database is not upgraded.

### Interface

- **Hover tooltips.** Rest the mouse on a footer chip, a composer or queue control, the to-do list header, an icon, or text that is cut short, and a tooltip explains it or shows the whole text. A paste token previews its first lines. Set `tooltips = false` under `[ui]` to turn tooltips off.
- **More in the workbench explorer.** A `CAUDRA` section lists your configuration, plans, memory notes, and in a local session your scratch files. A `FOLDERS` section holds any folder on this machine that you add with `Ctrl+X d` or with Add Folder on a section header. Each section folds, resizes, and scrolls on its own. Search, `Ctrl+P`, and source control still cover only the project.

### Permissions

- **Scopes that stop at a folder.** When you remember an answer for a command with a path in it, the scope choices now include its folders. For `gh api repos/acme/tool/issues`, you can allow `gh api repos/acme/tool/*` up to `gh api repos/*`. Patterns you write yourself can end the same way. Shells, interpreters, and other commands that run code get no such scopes.

### Agent

- **A reminder to act after repeated compactions.** When one request has been compacted twice and no work has started, Caudra reminds the agent to make its first change, write its plan, or report. Change the threshold with `work_nudge_compactions` under `[agent]`, or set it to `0` to turn the reminder off.

### Worktrees and Herdr

- **Labeled Herdr tabs.** When `/worktree` creates a worktree inside Herdr, the new tab and pane carry the branch name. If the session's old tab was the last one in its pane, Caudra now exits there and leaves the pane at its shell.
- **Fewer blocked moves.** A pending revert in an unrelated old conversation no longer stops a move to another directory or worktree, and `/worktree` names the session in the way before it creates the checkout. Moves also no longer fail with "storage writer operation timed out" while other Caudra processes are busy.

### Fixes

- **xAI sign-in works again.** Requests over an xAI login no longer fail with HTTP 426. If xAI raises its minimum client version again before the next release, set `CAUDRA_XAI_CLIENT_VERSION` to the version the error names.
- **`caudra -p` reports failures in its exit status.** A print run whose result is an error now exits with status 1 in every output format.
- **Plan mode with small models.** Models that never search for the `plan` tool can now save their plan.

## Caudra 0.2 Preview 3 {#v0-2-0-preview-3}

[v0.2.0-preview.3 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.3)

- Project memory is rebuilt around a summarized journal.
- GitHub sign-in for Copilot, and a Copilot Enterprise provider.
- Available updates show in every session.

> **Important**
>
> **Close every other Caudra session before you start Preview 3.** On its first start, Preview 3 upgrades the session database in your [state directory](/docs/configuration/#directory-layout) and needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them has the database open, Preview 3 does not start. It lists the sessions to close, and nothing is changed.
>
> The upgrade keeps your sessions and saves the old database as `caudra.db.v22.bak` next to `caudra.db`. Preview 2 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

### Project memory

- **Memory summarizes itself.** Notes the agent saves go into a journal, and a background job writes one-line summaries of older notes. The system prompt carries a view of at most 32 KiB, however many notes a project collects. The model opens a summary line to read the notes behind it, and searches the journal for anything the view leaves out.
- **Existing notes are imported automatically** the first time Caudra opens a project's memory. Note files are never modified. Tags are dropped, and the memory tool's `list` and `tags` actions are replaced by `view`, `zoom`, and `search`.
- **Summaries cost model usage.** By default the Memory job uses the same model as compaction, and it follows changes in `/model` from the next summary line. To spend less, bind Memory to a Fast model in `/model`. To turn summaries off, set `summarize_memory = false` under `[agent]`. Summaries are never written during one-shot `--print` runs, and the lifetime view in `/usage` lists their spend under `memory`.
- **`/memory` is an inspector.** It shows the view as the model reads it, expands each line into the notes it covers, and tracks the summaries being written.

### GitHub Copilot

- **Sign in with GitHub.** `caudra auth login copilot` now signs in through the browser with a one-time code. `--method import` still imports tokens from the gh CLI, the Copilot plugin, and the keyring.
- **Copilot Enterprise provider.** `caudra auth login copilot-enterprise --hostname company.ghe.com` signs in to a GitHub Enterprise host, and a personal account can stay signed in alongside it. Enterprise requests go only to `https://copilot-api.<host>`, never to public GitHub. For automation, set both `GH_COPILOT_ENTERPRISE_TOKEN` and `GH_COPILOT_ENTERPRISE_HOST`. The provider has not yet been tested with a live enterprise account, so please report any problems.

### Updates

- **Update banner.** When a newer release is available, a banner above the prompt names it and the update command for your channel, in every session. Close it with its `×` or `/dismiss-update` until the next start. Caudra never installs updates on its own. `CAUDRA_ENABLE_UPDATE_CHECK=1` checks GitHub immediately instead of using the result cached for 24 hours.

### Interface fixes

- **Guide and replace while work runs.** `Ctrl+X g` and `Ctrl+X x` now work whenever a prompt would queue, including while the agent waits on background tasks or workflows.
- **Usage limits.** When the Anthropic usage endpoint is rate-limited, `/usage` keeps the last figures and shows when they were fetched. Click the fetch time to refresh anyway.
- **Rhai highlighting.** Automation and workflow scripts are syntax-highlighted in the workbench, tool cards, diffs, and Markdown code blocks.
- **Workbench fixes.** Scrolling in Files, Git, and Search no longer snaps back to the selected row, and the Git graph refreshes after new commits, including in linked worktrees.

## Caudra 0.2 Preview 2 {#v0-2-0-preview-2}

[v0.2.0-preview.2 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.2)

- The installer gives clearer PATH guidance and installs fewer license files.
- Experimental decision features can use the OpenAI Decisions API.
- Background shell commands are syntax-highlighted.

### Installation and updates

- **Clearer PATH setup on macOS and Linux.** The installer prints a command you can use immediately, even when the installation directory is missing from `PATH`, with separate instructions for zsh, bash, and fish. It never edits your shell configuration, and it warns when it is installing for root rather than your regular account.
- **Fewer installed license files.** License notices now ship in a compact six-file bundle instead of thousands of individual files, with the detailed evidence kept in a compressed companion archive. Rollback can restore a saved installation even when the current license bundle is damaged.

### Experimental decision features

- **OpenAI Decisions support.** The decision engine can use the OpenAI Decisions API or a compatible local ninfer server with `decisions.protocol = "openai"`. Existing TypeSafe configurations keep their current protocol, and the decision inspector and status reporting show the selected protocol. Remote endpoints still require explicit consent.
- **Interactive question reminders.** The optional `question_tool_nudge` feature can remind the assistant to use the interactive question tool when it asks you to clarify, choose, or approve something in a prose reply. It does not invent an answer or grant approval.

### Interface fixes

- **Highlighted background commands.** Background shell results now syntax-highlight commands, matching the foreground shell display.
