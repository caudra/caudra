---
title: "Release notes"
description: "What changed in each Caudra release, newest first."
---

Each release lists what changed, newest first. After an update, the first interactive start shows the notes for every release since the one you ran before. Run `/changelog` to read them again, or `/changelog all` for every release. [`ui.changelog_on_update`](/docs/configuration/#ui-changelog-on-update) turns the automatic view off.

To install Caudra, follow the [Quick Start](/docs/quick-start/). To update, run [`caudra update`](/docs/cli/#caudra-update-caudra-rollback). Each release links to its GitHub page, which holds the downloads.

## Caudra 0.2 Preview 11 {#v0-2-0-preview-11}

[v0.2.0-preview.11 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.11)

- Sign in to one provider with several subscriptions and API keys, and let Caudra switch between them without breaking the prompt cache.
- Config edits keep your comments.

> **Important**
>
> **Close every other Caudra session before you start Preview 11.** On its first start, Preview 11 upgrades the session database in your [state directory](/docs/configuration/#directory-layout) to record which account served each turn, and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 11 does not start. It lists the open sessions to close instead, and nothing is changed.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `caudra.db.v30.bak` next to `caudra.db`. It also moves each saved provider login to `auth/accounts/<provider>/default.json`. Preview 10 and earlier cannot open the upgraded database or read the moved logins, and `caudra rollback` restores only the previous binary.

### Accounts

- **Several accounts per provider**: Each `caudra auth login` adds an account, such as a second Claude subscription or an API key, and `--name` names it. Signing in again as an account you already have refreshes it. `caudra auth status` lists the accounts of every provider with the quota they have left, `caudra auth use <provider> <name|auto>` chooses one, and `caudra auth rename` and `caudra auth logout <provider> [name] [--all]` manage them. See [Accounts](/docs/accounts/).
- **Automatic switching**: With two or more accounts, Auto keeps each conversation on its account and moves it to another before the account runs out. It moves only where the prompt cache is cold anyway, unless the account is nearly spent or refused. `ordered` tries the accounts in priority order, and `paced` uses the quota that resets soonest. `[<provider>.switching]` and `[[<provider>.accounts]]` in `providers.toml` set the thresholds, the order, and which API keys may take over from spent subscriptions. See [How Auto switches](/docs/accounts/#how-auto-switches).
- **Accounts in the TUI**: `/accounts` or `Ctrl+X i` lists, pins, adds, renames, orders, and removes accounts, with fresh quota readings for each 5-hour and weekly window. The status bar shows the account a conversation uses with its quota, each switch adds a transcript row with its cause and cache cost, and a spent account asks whether to switch or wait. `/usage` lists every account of the provider and splits lifetime spend by account.
- **Across providers**: With `[switching] providers = ["anthropic", "openai"]` in `providers.toml`, Auto moves a conversation to another provider's subscription once every subscription of its own runs out, on the first of your favorite models there, and back once the first provider has quota again. Each move adds a transcript row with its cause and cache cost. `/accounts` now lists every provider's accounts, and `o` and `Alt+↑↓` set the order. See [Across providers](/docs/accounts/#across-providers).
- **One run on another account**: `--account <provider>:<name|auto>` and `CAUDRA_ACCOUNT` choose an account for one run without saving the choice.
- **xAI sign-in in the TUI**: `/login` offers the xAI subscription sign-in beside an API key, and `caudra auth login xai --method api-key` stores a key.

### Changes

- **Keys in one variable are accounts**: A provider key variable holding several keys separated by commas becomes the accounts `env-1`, `env-2`, and so on, which Auto chooses between like any others, instead of rotating through them on every error.
- **A saved key wins over the variable**: A key `caudra auth login` saved is now tried before the provider's key variable.
- **Short rate limits wait**: A rate limit shorter than 30 seconds now waits on the same account, which keeps its prompt cache, instead of moving to the next key.
- **Logout keeps `providers.toml`**: `caudra auth logout` now deletes only the credential. An account entry in `providers.toml` stays and shows as signed out, and the next login without `--name` fills it.
- **Hidden key prompts**: `caudra auth login` no longer shows an API key as you type it.
- **Cards that open whole**: A `source_code_context` or `source_code_refs` card shows every row without a click and stays open after newer calls. A [Home Assistant](/docs/mcp/#home-assistant) or [Executor](/docs/mcp/#executor) call inside a `code` or `batch` card shows its whole card too, and a source code call inside a `batch` card no longer folds to one row. A click folds such a row, and in the auto `/view` such a card. The compact view and `ui.always_collapsed` still fold them.

### Fixes

- **Config edits keep your comments**: Saving a provider or decision endpoint, toggling an MCP server, or saving sandbox records now changes only the keys that changed. Comments on the other keys, quoting, inline tables, and table order stay as you wrote them, and a new table goes after the existing ones of its kind.

## Caudra 0.2 Preview 10 {#v0-2-0-preview-10}

[v0.2.0-preview.10 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.10)

- One `code` tool replaces `mcp` and `python_execution`, searches your past conversations, and can ask the decision engine to judge and rerank results.
- Two source code tools instead of five, renamed `source_code_context` and `source_code_refs`, that every model sees and that list call sites.
- Workflow `decide()` uses the OpenAI Decisions question shape.
- Add decision models in `/login` and switch them in `/model`, with Auto mode and the decision engine out of experimental.
- Pictures in the transcript.
- Search past sessions from the TUI with `/search-history`, beside `Ctrl+F` for the current chat.
- Notes on your answers to the agent's questions, and drag to copy in `/btw` and the question form.
- Goals stop when they make no progress, wait for CI or a deploy, and tell automations when they are stuck.
- Subscription image generation works again after GPT-5.5 left Codex.
- Fixes for Lua plugin cleanup, source indexing, shell-output filtering, and Nix builds.

> **Important**
>
> **Close every other Caudra session before you start Preview 10.** On its first start, Preview 10 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 10 does not start. It lists the open sessions to close instead, and nothing is changed.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `caudra.db.v28.bak` next to `caudra.db`. Preview 9 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

> **Warning**
>
> **Before you start Preview 10, rename `ui.tool_output_lines.python_execution` to `ui.tool_output_lines.code`.** The `code` tool replaces `mcp` and `python_execution`, with no alias. A `caudra.toml` that still sets the old key fails to load, with an error that names the new one. Permission rules, `agent.disabled_tools` entries, and system prompt profiles that name the `mcp` or `python_execution` tool no longer match anything, so name `code` in them instead. See [Code](/docs/code/).

> **Warning**
>
> **If you run a remote Workcell server, update it with Caudra and rename its `python_execution` tool group to `code`.** Workcell's Python tool is now `code` too, so `--tool-group python_execution` and `python_execution` in `WORKCELL_MCP_TOOL_GROUPS` fail at startup. Caudra requires a server that serves `code`, and refuses an older one. See [Remote workspaces](/docs/remote-workspaces/).

> **Warning**
>
> **If your workflows call `decide()`, rewrite their questions when you update.** Questions are now a list instead of a map keyed by ID, so each question names itself with `name`. `noul` questions become `predicate` questions, and `criteria` becomes `choices` for a choice and `levels` for a score. Answers come back as `answers`, a list in question order, and `by_name`, keyed by name, and a predicate's probability is now in `probability` rather than `noul`. A workflow that still passes the old shape fails at its first `decide()` call with a catchable error, and so does a run started before the update when you resume it, even at a call it had already answered. See [Typed decisions](/docs/workflows/#typed-decisions).


> **Warning**
>
> **If your config, profiles, or remote Workcell use the code-graph tools, update them together with Caudra.** `code_context` and `code_refs` are now `source_code_context` and `source_code_refs`, and `code_map`, `code_impact`, and `code_expand` are gone: use `source_code_context` where you used `code_map` or `code_expand`, and `source_code_refs` with `depth` where you used `code_impact`. The profile tool group `code_graph` is now `source_code`, and `[decisions.features] code_context` is now `source_code_context`. A config or profile that still names an old tool, group, or feature is rejected as unknown. Your sessions, `/tools` usage, decision log, and saved permission rules move to the new names the first time Caudra starts, and sessions that called a removed tool still open and show those calls. A remote Workcell must run the same release as Caudra, or connecting fails with an incompatible tool catalog, and its `--tool-group code_graph` is now `--tool-group source_code`. The SDK's tool kinds are now `SourceCodeContext` and `SourceCodeRefs`. See [Code Intelligence](/docs/tools/#code-intelligence).

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

- **One `code` tool replaces `mcp` and `python_execution`.** The model runs all of its Python in `code`: computation, MCP server calls, history search, and the decision helpers. It is offered in every session, with or without MCP servers, and its definition never changes, so the prompt cache holds. Variables persist between runs of the same agent, and `reset: true` starts a run empty. A model that still calls `mcp` or `python_execution` gets an unknown-tool error. `/tools`, `caudra tools`, and `/mcp` mark MCP tools `via code`. See [Code](/docs/code/).
- **Computation runs where snippets run.** Python now always runs on your machine, also in sessions on a remote Workcell. A run has 30 seconds of computation, where `python_execution` stopped at 5 seconds unless the model asked for more, and the clock stops while a snippet waits for a call. `timeoutSec` is gone. One agent's runs go one at a time, so the model gathers parallel work in one snippet, and a `code` call starts once the model's response is complete. See [Where snippets run](/docs/code/#where-snippets-run).
- **Search your past conversations.** `history.search()` finds what you wrote, what the model answered and reasoned, and what its tools did, in this conversation, this project, another project, or every stored session, compacted turns and subagents included. Each hit carries a snippet around the matched words and a reference that `history.read()` opens in full, and `history.sessions()` lists sessions. A full-text index in the session database fills in the background while the TUI is open, and each search catches it up first. Searching this project never asks. Another project or all projects ask first. The prompt shows the call with its arguments and which session or project's history it reads, and offers to always allow that history. See [Search past conversations](/docs/code/#search-past-conversations).
- **Reranked history search.** With a decision endpoint active, set `[decisions.features] history_search = "enforce"` and the engine judges up to 64 keyword matches against the query, drops those below `thresholds.relevance_floor`, and returns the 10 most relevant. Its four requests go out at once, and each waits up to four times `rerank_timeout_ms`, so an engine that answers one request at a time can still judge the whole pool. Otherwise a search returns 20 matches in keyword order. The feature is off by default, and `shadow` logs what it would have kept. See [Ranking](/docs/code/#ranking).

### MCP

- **Decisions and reranking in snippets.** With a decision endpoint active, a `code` snippet can call `decide()` for typed answers about its data, and `rerank()` to keep only the candidates relevant to a query, best first, for up to 256 candidates. A snippet can rank 200 issue titles and return the ten that matter, so the rest never reach the model. The model is told how to write questions and queries that get reliable answers, such as naming the field each question judges and describing every option. Both work with or without MCP servers. Calls need no approval, work in plan mode, show as rows on the card, and appear in the decision log under the feature `code`. See [Decisions and reranking](/docs/code/#decisions-and-reranking).
- **Decision answers you can read at a glance.** A `decide()` row on the `code` card shows its verdicts on one line, and always shows each question below it with a bar per option, the likeliest highlighted, and a warning when the winner is uncertain. A `rerank()` row always shows the candidates it kept, best first, under the labels they were given. Click the row's footer to see the JSON the snippet received; selecting any part of the row copies that JSON, in either view. In `/decisions`, Recent draws logged answers the same way, and `j` switches them to JSON.

### Decisions

- **Decision models in `/login` and `/model`.** `/login` has a **Decision models** section with TypeSafe Jev, OpenAI Luna, and a custom endpoint, which asks for a name, the protocol, the base URL, the model, and a key. It asks before it saves plain HTTP to a host other than numeric loopback. A new endpoint becomes the active one. In `/model`, the **Decisions** row of the Jobs section switches endpoints, turns decisions off, or adds one, and `/decision-model` opens it directly. `/decision-next` or `Ctrl+X d` moves to the next endpoint. A switch applies to every open session at once, and other Caudra processes pick it up when they start. While an endpoint is active, the status bar shows `[decisions: <name>]`. See [Decision endpoints](/docs/providers/#decision-endpoints).
- **Decision endpoints from the command line.** `caudra auth login --decision` takes `typesafe`, `openai`, `custom`, or a configured name, and the `caudra auth login` menu lists decision models too. `caudra auth logout --decision <name>` removes an endpoint and its key, and `caudra auth status` lists every endpoint with its key source. `caudra decisions list` shows the endpoints, `caudra decisions use <name|off>` switches, and `caudra decisions status` reports the active `endpoint`. See [CLI](/docs/cli/#caudra-decisions).
- **A key for each endpoint.** Login saves the key for one endpoint in the state directory. Without a saved key, Caudra reads the variable that the endpoint's `api_key_env` names. The presets name `TYPESAFE_API_KEY` and `OPENAI_API_KEY`, so a key you keep in the environment still works.
- **Auto mode and the decision engine are no longer experimental.** `/auto`, `--auto`, SDK `--permission-mode auto`, `/decisions`, and `caudra decisions` work without a switch, and `always_auto = true` and sessions saved in Auto start in Auto again. With no active endpoint, Auto runs the calls that deterministic checks clear and prompts for a line that needs screening. Every decision feature stays off until you turn it on. See [Auto mode](/docs/permissions/#auto-mode).

### Providers

- **`caudra auth login` keeps your comments in `providers.toml`.** Login and logout now replace only the table they change, and leave the rest of the file and its comments as you wrote them.

### Workflows

- **`decide()` takes OpenAI Decisions questions.** A choice option or a score level can carry a description, as a map with `value` or `label` and `description`, and each answer lists the probability of every option or level. Both decision protocols return the same shape. `rerank()` is unchanged.

### Transcript

- **Pictures in the transcript.** `view_image` and `image_generate` cards, Markdown images that point at local files, and prompts with attached images now show the picture as a thumbnail. Terminals that support Kitty graphics get the full-resolution picture, and other terminals get coloured blocks, two by two pixels to a cell, or two by three with `inline_images = "sextants"` in terminals and fonts that draw sextants. Click a picture to open it at full size, then press `o` to open it in your system viewer, `w` to open it in the workbench, or `y` to copy its path. Images attached to a draft show as `[Image #1]` chips you can click, and the workbench shows image files instead of the binary file notice. Markdown images are never fetched from the network. Set `[ui] inline_images` to `"blocks"`, `"sextants"`, `"kitty"`, or `"off"` to choose. See [Images](/docs/markdown/#images).
- **Search past sessions.** `/search-history` opens the search on this project's saved sessions, and `Ctrl+F` opens it on the current chat, which works as before. `Tab` switches between This chat, Project, and All projects and keeps what you typed. The history tabs search whole words, list each match with its session and age, and preview the selected item with its neighbours. Words can carry `kind:`, `tool:`, `since:`, and `until:` filters, and `Ctrl+S` sorts newest first. With the `history_search` decision feature on `enforce`, results are reranked once you stop typing, and `Ctrl+R` returns to keyword order. `Enter` scrolls to the match in this session, or opens the other session and scrolls there. See [Search past sessions](/docs/sessions/#search-past-sessions).

### Questions

- **Add a note to your answers.** On a question the agent asks, `F5` opens a note for anything the options and your answers leave out, even while you type an answer. The note shows on every tab and in the review, and a click edits it. `Enter`, `Esc`, or `F5` again leaves it with the text kept. It reaches the agent with your answers, and with the dismissal when you press `Esc` on the form. `Ctrl+C` still stops the agent, and moves the note to the composer to start your next message. Questions from MCP servers and consent requests have no note. The form's hints now start with `Enter submit`, then `F2 Ask /btw`. See [Keybindings](/docs/keybindings/#form).

### Selection

- **Drag to copy in `/btw` and the question form.** Dragging over a `/btw` thread copies the passage when you let go, the way the transcript copies: within one answer as its Markdown, and across questions and answers as sections headed `User`, `Assistant`, `Thinking`, and `Error`, without the footer, the follow-up field, or a line break at every wrapped row, and the passage stays selected while the answer streams and scrolls. `Ctrl+Y`, `Ctrl+C`, and the footer's copy take the selection before the whole answer. In the question form, a drag starts anywhere, including on an option, and copies instead of picking what it ends on. A drag in the answer box selects and copies its text, and `Ctrl+C` copies a selection before it stops the agent. The form's border is no longer copied with the text. See [Commands](/docs/commands/) and [Keybindings](/docs/keybindings/#form).

### Goals

- **A goal that stops getting anywhere stops its run.** When three turns in a row make no progress, the run ends with the goal still active instead of spending continuations until the limit. A turn makes no progress when the evaluator saw nothing new, when it repeats an earlier turn exactly, or when the agent stops to ask you something. Progress is read from the conversation, so work on a remote system counts once a tool result shows it. Autopilot reviews the stall like a loop cap: a continue buys one more turn, and its review streak still leaves the fourth continue in a row to you. Headless clients get a `stalled` goal event. See [Commands](/docs/commands/).
- **A goal waiting on something outside the session waits instead of stalling.** When the evaluator sees the goal waiting on a change that comes by itself, such as a CI run or a deploy, the run ends and the goal is checked again after a minute, then twice as long each time up to 30 minutes, for eight waits, about two hours. Then it stalls as before. A question to you never waits, and a message from you ends the wait. The wait is saved with the session, so a resumed session checks in when it is over. Headless clients get a `waiting` goal event. See [Commands](/docs/commands/).
- **Automations hear about a stuck goal.** A `goal_finished` trigger that lists `stalled` in `verdicts` fires when a run stops on a goal that made no progress or used its continuations. The goal stays active, so a reply can still finish it. See [Automations](/docs/automations/).

### Image generation

- **ChatGPT subscription image generation uses a supported model.** Image requests now run through GPT-6.1 Sol instead of GPT-5.5, which OpenAI retired from Codex with ChatGPT sign-in on October 14. This fixes the unsupported-model error without changing your configured image model or switching to API billing.

### Source code tools

- **Two source code tools instead of five, under new names.** `source_code_context` (formerly `code_context`) finds the code a task touches, and `source_code_refs` lists who calls a symbol. `source_code_refs` takes `depth` to show what a change could reach, with a count per hop and how many of the reached symbols are tests, which is what `code_impact` did. Both tools are always in the request for every model, so a small model no longer has to search for them first. The system prompt names them with one line each and points the model at `source_code_refs` instead of grep for callers.
- **Call sites in `source_code_refs`.** Each caller row lists the lines that make the call, up to 16 per row with a count of the rest, so one call finds every place a change must touch. Callees come in the order the body calls them. A call linked by its name alone, because the receiver's type was never seen, is marked `~name` and no longer moves the ranking.
- **Standard library calls stay out of the graph.** In Rust, a call such as `.clone()`, `Vec::new()`, `Arc::new()`, or `std::mem::take()` no longer links to a symbol of the same name in your tree. A common method name links only when the calling file names the type that defines it, and `source_code_refs` says how many same-name calls it left unlinked. A bare call reaches only free functions, and a method call reaches only methods.
- **`source_code_context` lifts only the names you write.** A name in the task lifts its symbol only when written as code, such as `build_csr`, `` `parse` ``, `a::b`, or `run()`. An ordinary word such as "renew" no longer pulls in every `new`, and a symbol that matches only through its file's path, such as a constructor in a file the task names, ranks below one whose own name matches.

### Tools

- **`/tools` counts calls made inside `batch`.** Each call a batch makes now has its own row with its own outcome, time, and tokens, and the `batch` row keeps the wall time of the whole batch. The token total is unchanged.

### Fixes

- **A session with a stored tool card this build cannot read opens again.** Such a card, like an `mcp` card from Preview 9, is skipped, and its call shows the text the model received. Before, the whole session failed to open.
- **Lua job cleanup survives cancellation and deadlines.** Releasing an abandoned `jobwait` during garbage collection no longer aborts the process, including when the plugin host shuts down.
- **Shell output keeps unrelated subcommands intact.** Updated bundled filters no longer match commands merely because their names start with a filtered subcommand.
- **PHP indexing handles clone arguments and partial application.** File indexes and code graphs no longer report parse errors for these expressions.
- **Go type aliases appear in file indexes and code graphs.** Declarations such as `type Alias = Target` are now included as type definitions.
- **SVG source icons enforce pixel-buffer limits across nested effects.** Allocation checks now include linked masks and clips, retained filter results, and drop-shadow scratch buffers. Over-budget icons are skipped.
- **HTML conversion preserves code layout and lazy-loaded images.** Code blocks retain blank lines and image placeholders resolve correctly. Equally scored article candidates now follow document order.

### Builds

- **Nix evaluation works without building the worker source first.** The pinned Python worker source is fetched and verified during evaluation, without import from derivation.
- **Workcell container builds include all attribution sources.** The Debian 13.7 image fetches both locked dependency graphs before generating its offline license bundle.

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
> **Before you start Preview 9, delete `defer_tools` and `always_load` from `mcp.toml`.** Both tuned tool search for MCP tools, which code mode replaces. A global or project `mcp.toml` that still sets either fails to load: Caudra starts without that file's servers, and `/mcp` shows an error that names the key.

### MCP

- **Complex numbers in Python.** The Python runtime is now Monty 1.1.0. Python calculations, MCP snippets, and Lua interpreter calls support complex literals and `complex()`. Complex results use a string representation where JSON cannot preserve their type.
- **Code mode.** The model now reaches your MCP servers by writing Python in one `mcp` tool, instead of receiving a definition for every tool. A snippet can call several tools at once and filter or join their results before the model reads anything, so a list of 500 issues can come back as three lines. The model learns your servers and their tools from a short list in the conversation, and `search()` finds the tools the list leaves out. See [Code mode](/docs/mcp/#code-mode).
- **MCP servers keep the prompt cache.** The tool definitions stay the same while a server connects, fails, or changes its tools, and when you switch to plan mode, so none of these makes the provider read the whole conversation again at full price. See [The prompt cache](/docs/configuration/#the-prompt-cache).
- **Approvals work as before.** Each tool call in a snippet asks on its own, and permission rules, `agent.disabled_tools`, and profiles still name a tool `server.tool`. A call you decline raises an error the snippet can catch. Plan mode and read-only agents still allow only the read-only tools of recognised servers. See [MCP tool calls](/docs/permissions/#mcp-tool-calls).
- **`mcp_resource` is gone.** The model reads a server's resources with `resources()` and `read_resource()` in `mcp`. Reads still ask first, and reads you allowed for `mcp_resource` ask once more. See [Resources](/docs/mcp/#resources).
- **Servers with a code mode of their own.** Add `?codemode=false` to the URL of Cloudflare's MCP server, or `?mode=passthrough` to Executor's, so the model calls their plain tools rather than writing code for their code tool. See [Servers with a code mode of their own](/docs/mcp/#servers-with-a-code-mode-of-their-own).
- **Earlier sessions resume.** A conversation from Preview 8 or earlier picks up as before. When the model calls an MCP tool by its old `server__tool` name, the error tells it to use `mcp`.
- **`/tools` and `caudra tools`.** An MCP tool shows `via mcp`, or `search-only` when a profile makes it `lazy`, instead of a token cost. `tool_search` now loads only built-in tools.
- **`/mcp` shows how the model reaches each server.** A server's row gives the name the model calls it by in `mcp` and how many of its tools a profile leaves search-only. An Executor server whose URL lacks `?elicitation_mode=native` or `?mode=passthrough` says what each one changes.
- **A missing code worker is reported up front.** When MCP servers are configured but Caudra has no code worker to run snippets on, the TUI says so as it starts and `/mcp` shows it above the server list, rather than waiting for the model's first `mcp` call to fail. See [Where snippets run](/docs/mcp/#where-snippets-run).
- **`/tools` counts every server call.** Its recorded views give each call a snippet makes a row named `server.tool`, with its calls, errors, and time, alongside the `mcp` run that made it. See [What the tools cost](/docs/token-economy/#what-the-tools-cost).
- **Snippets are checked before they run.** A misspelt tool or an argument of the wrong type stops a snippet before any server call, naming the closest tool and the line, in the agent's later runs as well as its first. A tool takes one dict of arguments as well as keywords. A snippet that is not valid Python says why, such as `return` outside a function or JavaScript instead of Python. See [Results and errors](/docs/mcp/#results-and-errors).
- **Slips with one meaning are fixed before a snippet runs.** A standard module the snippet uses without importing it, such as `asyncio` for `asyncio.gather`, is imported. `await` before `search()` or `servers()` is dropped, `return` on the snippet's last line is dropped, and a Markdown fence around the whole snippet is removed. `asyncio.gather(..., return_exceptions=True)` now runs, settling each failed call as its exception. Errors still name the line the model wrote, the card lists each fix, and the model reads what it did wrong in the result. See [Results and errors](/docs/mcp/#results-and-errors).
- **Snippets run in their own process.** An `mcp` snippet runs in a separate worker process with no filesystem, network, subprocess, or environment access, rather than inside Caudra. It reaches only the MCP calls Caudra answers, each permission-checked, and a crash ends only the worker. See [Where snippets run](/docs/mcp/#where-snippets-run).
- **File revert records an `mcp` run from its first call that may change files.** A run that calls no server, or only tools a recognised server marks read-only, leaves no record. See [Recording](/docs/sessions/#recording).

### Models

- **Gemini 4 Argon.** `google/gemini-4-argon` is the Google Best model and the model Caudra picks when you use Google without choosing one, in place of `gemini-2.5-pro`. It costs $2 per million input tokens and $10 per million output tokens, with cache reads at $0.10, until November 30, 2026. Thinking takes `low`, `medium`, and `high`, and thinking `off` runs at `low`, because Argon always thinks. Argon is not on Google's free tier, so it needs a key on a project with billing enabled. On the free tier, pick `google/gemini-3.8-flash`. When Google gives a key no quota for a model at all, Caudra now says so at once instead of retrying for two and a half minutes. Caudra asks for at most 65,536 output tokens, the most one streaming request returns. If you raise `max_output_tokens` past that, an answer Gemini stops at the cap is now reported as cut off instead of complete. See [Google](/docs/providers/#google).
- **Gemini 3.8 Flash and 3.6 Flash.** Both cost $0.75 per million input tokens and $3.75 per million output tokens until December 31, 2026. 3.8 Flash takes thinking levels from `low` to `high`, and 3.6 Flash from `minimal` to `high`.
- **Gemini 3.5 Flash-Lite is the Google Fast model.** It replaces `gemini-2.0-flash-lite`, which Google shut down, so session titles, completion-goal checks, and other Fast work on a Google session run on it unless you bound those jobs to a model. Thinking `off` runs at `minimal`.
- **Gemini 2.5 Pro and 2.5 Flash leave Caudra's model table.** Google stopped serving them to new API keys and points to `gemini-3.8-flash` instead. A key that still has them can use them, but Caudra no longer shows their cost.
- **Gemini price changes on their dates.** Argon moves to $4 and $20 per million input and output tokens on December 1, 2026, and 3.8 Flash and 3.6 Flash move to $1.50 and $7.50 on January 1, 2027. Caudra counts each turn at the price in force when it ends, from 00:00 UTC on those days, without an update. The providers page lists both prices with the date.
- **Gemini on Copilot.** Copilot and Copilot Enterprise add `gemini-3.8-flash` and `gemini-4-argon`, at GitHub's introductory prices, and drop `gemini-3.5-flash`, `gemini-3.6-flash`, and `gemini-3.1-pro-preview`, which GitHub no longer serves.
- **Google models answer again.** Every request to a Google model failed with `Invalid JSON payload received. Unknown name "$schema"`, because Caudra's tool definitions name the JSON Schema version they follow, and the field Caudra sent them in refuses it. Caudra now sends tool definitions in the field that takes standard JSON Schema, so tool definitions that use `$ref`, `oneOf`, or `const` work with Google too. It rewrites the few shapes Gemini still refuses, such as a `required` list that names an undeclared property.
- **Gemini 3 and 4 accept tool calls from another model.** Google refuses a request when a tool call in the current turn carries no thought signature, which happened after you switched to Gemini 3 or 4 in the middle of a turn. Caudra now marks such a call the way Google documents, and keeps every signature Gemini issued. Aliases without a version, such as `gemini-flash-latest`, count as current models.
- **Claude Sonnet 4.6 answers at full length.** Caudra now lets `anthropic/claude-sonnet-4-6` write up to 128,000 output tokens, the limit Anthropic documents, instead of 64,000.
- **Retired Claude models leave the model table.** Anthropic retired Claude Sonnet 4 and Claude Opus 4 on June 15, 2026, and Claude Opus 4.1 on August 5, 2026, so Caudra drops `claude-sonnet-4`, `claude-opus-4-0`, and `claude-opus-4-1` from its model table. Use `claude-sonnet-5-5` or `claude-opus-4-8` instead, as Anthropic recommends.
- **Retired Codex models leave the model table.** OpenAI shut down `gpt-5.1-codex`, `gpt-5.1-codex-max`, `gpt-5.1-codex-mini`, and `gpt-5.2-codex` on July 23, 2026, so Caudra drops them too. Use `openai/gpt-5.6-sol`, or `gpt-5.6-terra` in place of `gpt-5.1-codex-mini`.
- **Grok 4.7.** `xai/grok-4.7` is the xAI default, in place of `grok-4.6`, and the model Caudra picks when you use xAI without choosing one. It costs $2 per million input tokens and $6 per million output tokens, with cache reads at $0.50. Once a prompt reaches 200,000 tokens, every rate doubles for the whole request, as xAI bills it. Thinking takes `low` to `xhigh`.
- **Grok 4.7's long-prompt rate starts at the right token.** Caudra's cost tracking bills `xai/grok-4.7` at the long-context rate from a prompt of exactly 200,000 tokens, the boundary on xAI's model page, and Caudra's check of its model list against models.dev no longer reports that boundary as a mismatch.
- **Copilot follows GitHub's model list.** Copilot and Copilot Enterprise add `grok-4.7`, at $2 per million input tokens and $6 per million output tokens. They drop `kimi-k2.7-code`, `mai-code-1-flash-picker`, `claude-opus-4.7`, `claude-opus-4.6`, `claude-opus-4.5`, and `claude-sonnet-4.5`, which GitHub retired. `claude-sonnet-4.6` stays, because GitHub still serves it on annual Copilot Pro and Pro+ plans.
- **Mistral Large 4 and GLM 5.3 on Mistral.** `mistral/mistral-large-4`, in public preview, costs $0.68 per million input tokens and $2.09 per million output tokens at its launch price, and $1.36 and $4.18 from October 20, 2026, when Caudra switches on its own. `mistral/glm-5-3` replaces `glm-5-2` at the same price. Mistral retires GLM 5.2 on October 31, 2026.
- **DeepSeek V4.1 Flash.** `deepseek/deepseek-flash` is the model Caudra picks when you use DeepSeek without choosing one, in place of `deepseek-v4-flash`, which DeepSeek retired and now serves with V4.1 Flash. It reads images and costs $0.15 per million input tokens and $0.60 per million output tokens off-peak, twice that at peak hours. Caudra bills `deepseek-v4-flash` at the same price, and sends V4.1 Flash the empty reasoning it needs on earlier turns so that thinking requests are not refused.
- **GLM-5.3 on Z.AI.** `zai/glm-5.3`, Z.AI's new flagship, is the Z.AI Best model and the model Caudra picks on the pay-as-you-go plan when you don't choose one, in place of `glm-5.1`. It costs $1.40 per million input tokens and $4.40 per million output tokens, takes a 1M-token context, and always thinks. The Coding plan keeps `glm-5-code`. `glm-5.3-flash` and `glm-5.3-flashx` join as small models that read images. Caudra now bills `glm-5.2` and `glm-5.1` at Z.AI's current $1.40 and $4.40, up from $1 and $3.20.
- **No Google sign-in.** A Google AI Pro or Ultra plan covers AI Studio in the browser, and Google bills API use separately. Google's terms forbid third-party tools from using the sign-in of Gemini CLI or Antigravity, so Caudra uses an API key for Google. The providers page now says so.

### Modes

- **Forks keep their permission mode.** A fork now inherits the parent's current Ask, Auto, or YOLO mode, including when it opens in another terminal tab or moves to a new worktree. Conversation-specific approval rules still start empty.
- **Switching mode or model applies once the session settles.** A `Tab` toggle, and the model it brings back, waited for your next message even with nothing running, so the status bar kept its `[BUILD→PLAN]` arrow after `/compact`, and `/context`, `/btw`, and tasks you started still used the old mode and model. The switch now takes effect as soon as nothing runs and nothing waits to report. While work runs, it still waits for that work to finish or your next message. See [Selecting Plan while work runs](/docs/queue/#selecting-plan-while-work-runs).

### Worktrees

- **`/worktree close` brings the work back.** Closing a worktree is the reverse of creating one. It fast-forwards the main checkout's branch to the worktree's commit, moves the worktree's sessions back, carries its uncommitted changes into the main checkout, and removes it, keeping the branch. The removal confirmation, which `/worktree close`, `/worktree remove`, and `Ctrl+D` in `/worktree` open, offers it first and says what it brings back, or why it cannot: when the main checkout has commits of its own, or changes that would mix with the worktree's. If the changes still do not apply, they stay in the worktree and the worktree is kept. See [Closing a worktree](/docs/worktrees/#closing-a-worktree).
- **Every tab comes along when you leave a worktree's workspace.** Closing or removing a worktree from its own Herdr workspace or Orca terminal resumed only the focused session, in a pane split beside the repository's. Every session open in Caudra now continues in a tab of its own in the repository's workspace, labelled with its title, and the focused one keeps the focus. See [Removing a worktree](/docs/worktrees/#removing-a-worktree).
- **Removal checks first.** A pending revert or an active workflow of a session in the worktree stopped its removal only after its changes had been stashed. Removing and closing now refuse before they stash, merge, or remove anything, as creating a worktree does.

### Pickers and lists

- **`/model` opens on your favorites.** With favorite models, `/model` and `Ctrl+X m` put the cursor on the current model's favorite row, or on the first favorite, instead of on the Chat job, so you no longer move past every job row first. Without favorites, the picker opens on Chat as before. See [Model jobs](/docs/providers/#model-jobs).
- **`Home` and `End` reach the ends of a list.** In `/model`, `/sessions`, and every other picker, `Home` and `End` jump to the first and last row while the filter is empty. Once you type a filter they move the text cursor again, and `Ctrl+Home` and `Ctrl+End` still jump to the ends. In the workflow and automation inspectors, `PageUp`, `PageDown`, `Home`, and `End` now move the run or automation list while it has the focus, instead of scrolling the detail beside it. See [Pickers](/docs/keybindings/#pickers).

### Usage

- **Spend by purpose for the session.** `/usage` now shows a Per purpose table for the current session too, beside Per model: what went to the conversation and what to titles, goal checks, compaction, memory summaries, and Caudra's other work. A session counts purposes for turns from Preview 9 on. See [Commands](/docs/commands/).
- **Purposes show in full.** The lifetime view cut Per purpose off after 8 rows, so a ninth purpose hid behind `... and 1 more`. Purposes now always show whole, and no list hides a single row behind a line of its own.
- **Long lists open in place.** In the lifetime view, click `... and N more` under providers, models, projects, or months to show the rest, and `show less` to fold it again. Press `e` to open or close every list at once. See [Lifetime spend](/docs/token-economy/#lifetime-spend).

### Questions

- **Questions that don't stop the agent.** The agent can ask a question that it doesn't need answered for its next steps, such as a preference or a name, and keep working. The form docks as usual. Press `Esc` to put it aside: it waits in a `[? N]` chip in the footer that counts unanswered questions, and `Ctrl+X u`, for unanswered, or a click on the chip brings it back with what you had picked. Your answer reaches the agent when you give it, even after the agent has finished its turn, and the oldest open question docks again when the agent goes idle. Up to three can be open at once, `F4` moves to the next, and `/tasks` lists them as waiting for your answer. Turn this off with `deferred_questions = false` under `[agent]`. See [Deferred questions](/docs/sessions/#deferred-questions).
- **Answer later.** On a question the main agent waits on, `F4` puts it off: the agent gets a receipt instead of an answer and carries on, and the form waits off the dock with your picks until you answer it. `Ctrl+C` on any question still stops the run, and stopping the run withdraws its open questions.

### Tasks

- **Tasks run on the favorite you name.** When you ask for a model by a short name, as in "ask sonnet to review this", the task now runs on your favorite model that matches the name, even when a newer model or another provider matches too. While the main agent can choose a task's model, it is also told your favorites with their lane, the jobs you bound to them, and the thinking levels they take, and hears again when you star or unstar one. See [Models and reasoning](/docs/commands/#models-and-reasoning).

### Autopilot

- **No continuation limit.** `max_continuations = 0` under `[autopilot]` lets an engagement run until its goal ends, its cost or time limit runs out, or you stop it, so a session that an automation keeps busy every day can stay on autopilot. The footer chip then shows the count without a limit, and SDK clients read `max_continuations` as `null`. See [Budgets](/docs/autopilot/#budgets).
- **An audit log of what autopilot did.** The status panel gains a Log tab, which `/autopilot log` opens at any time, engaged or not. It lists every decision of the session under its engagement, with what the delegate was asked, such as the exact command of a permission request, what autopilot said, its reason, the model and cost, and what happened next: whether the action applied, you answered first, you took the card, or you answered after autopilot left it to you. Press `f` to filter, `y` to copy an entry, and Enter to jump to its row in the transcript. Click a decision row in the transcript to see what autopilot said and what happened next, and the start and end of each engagement come back when you resume. Before, the panel showed only the last 20 decisions of the running engagement in one line each, and a decision row kept only the action's name. Decisions made before Preview 9 keep their one-line record. SDK clients read the log with the new `autopilot_log` control, and `decided`, `engaged`, and `ended` events carry the new fields. See [The log](/docs/autopilot/#the-log).
- **Autopilot answers the agent's questions.** While engaged, autopilot never answered a `question` form from the main agent, or a server's form during one of its tool calls, so they always waited for you. It now answers them, as Preview 8 described.
- **The delegate sees your overrides.** When you answered a decision differently from autopilot, the delegate's next decisions see your answer, so it can follow your lead.

### Goals

- **A log of every goal check.** The `/goal` panel gains a Log tab, which `/goal log` opens. It lists every goal of the session and every check of it, with when it ran, how long it took, its status, the evaluator model, its cost, and its reason. Besides met, not met, and impossible, a check can read as reconsidered when a second opinion overruled an impossible verdict, failed, skipped by the goal prescreen, deferred, or stale when the goal changed while it ran. Press `f` to filter, `y` to copy an entry, and Enter to jump to its row in the transcript. Before, the panel showed only the latest verdict, and the transcript rows of a goal's checks were gone after a resume. They now come back. SDK clients read the log with the new `goal_log` control, and goal events carry each check's `seq`, goal number, and times, with a new `skipped` event. See [The goal log](/docs/commands/#the-goal-log).
- **Goal checks survive an interrupted reply.** After you interrupted the agent while it was thinking, every later goal check on a Claude model failed with `thinking.signature: Field required`, and the goal could never be met. Checks now judge the conversation's messages and tool results without the agent's reasoning, which also keeps it away from an evaluator on another model or provider and leaves more of the check's budget for evidence. A goal whose checks were failing is checked normally again once you update.

### Side questions

- **`/btw` answers when the model reaches for a tool.** A model that tried to call a tool in `/btw` used to stop with `Stopped: the model tried to call a tool`, often before it had said anything. Caudra now tells it that tools are unavailable and asks again, so it answers from the conversation and says what it could not confirm. It stops only if the model keeps calling tools after two reminders. See [Commands](/docs/commands/).

### Command line

- **Updates on macOS no longer stall.** Updating a Mac that still had the large license bundle from Preview 1 stayed on `downloading caudra …` for about four minutes. The installer checks the previous bundle before replacing it, and its JSON reader grew slower with every character of that bundle's 2 MB manifest under the `awk` that ships with macOS. It now reads the manifest in under a second, and a fresh install reads the GitHub release list faster too.
- **Updates use your GitHub token.** `caudra update` and the update notice now send `GITHUB_TOKEN`, or `GH_TOKEN`, when they ask GitHub for releases, the same as the installer. Behind a shared IP that had used up GitHub's unauthenticated quota, they failed with `server returned HTTP 403`. Without a token, that refusal now says to set one, in `caudra update` and in both installers.
- **Download progress.** The shell installer shows curl's progress bar for the release archive when it runs in a terminal, so updating to Preview 9 shows it already. From Preview 9 on, `caudra update` downloads the archive itself and shows a bar with size, speed, and time left. It checks the archive against the release checksums before it saves your current version, and if the download fails, the installer downloads the archive instead.

## Caudra 0.2 Preview 8 {#v0-2-0-preview-8}

[v0.2.0-preview.8 on GitHub](https://github.com/caudra/caudra/releases/tag/v0.2.0-preview.8)

- Claude Haiku 5.5 is the Anthropic Fast model.
- The experimental decision engine reranks memory notes, tool search, retained tool output, and code context, and the transcript shows what each decision changed.
- Workflows gain `rerank()`.
- Favorite and 10 recent models in `/model`, and tasks run on the model you name.
- Forks open in a new Herdr or Orca tab, and tabs are named after the session.
- New sessions can open in build mode, and `/sessions` can delete the current session.
- The wheel scrolls lists instead of moving their selection.
- Fixes for session moves, Orca's bell, and `caudra update` output.

> **Important**
>
> **Close every other Caudra session before you start Preview 8.** On its first start, Preview 8 upgrades the session database in your [state directory](/docs/configuration/#directory-layout), and the upgrade needs exclusive access. This includes other terminals, editor sessions over ACP, and `caudra storage` commands. While any of them still has the database open, Preview 8 does not start. It lists the open sessions to close instead, and nothing is changed.
>
> The upgrade keeps your sessions and saves a copy of the database from before it as `caudra.db.v24.bak` next to `caudra.db`. Preview 7 and earlier cannot open the upgraded database, and `caudra rollback` restores only the previous binary, not the database.

> **Note**
>
> **If you raised `decisions.timeout_ms` for a slow decision engine and use `tool_search`, set `rerank_timeout_ms` as well.** Tool search now uses that deadline, which defaults to 2 seconds.

### Models

- **Claude Haiku 5.5.** `anthropic/claude-haiku-5-5` costs $0.10 per million input tokens and $0.50 per million output tokens, with cache reads at $0.01. Once a prompt passes 100k tokens, cached tokens included, Anthropic bills the whole request at five times those rates, and Caudra counts its cost the same way. Caudra runs it at the 372k working window of other recent Claude models, and `claude-haiku-5-5-1m` opens the full 1M window. Thinking takes effort levels from `low` to `max`, and thinking `off` turns its reasoning off. Copilot and Copilot Enterprise list it as `claude-haiku-5.5`. There, as with Copilot's other Claude models, `off` runs at the lowest level. See [Anthropic](/docs/providers/#anthropic).
- **Haiku 5.5 is the Anthropic Fast model.** It replaces `claude-haiku-4-5` as the curated Fast model, so on an Anthropic session, session titles, completion-goal checks, and other Fast work run on it unless you bound those jobs to a model. Bind Fast to `anthropic/claude-haiku-4-5` in `/model` to keep the older model. See [Model jobs](/docs/providers/#model-jobs).
- **Sonnet 5.5 cache reads cost less.** Anthropic halved the cache read price of Claude Sonnet 5.5 to $0.10 per million tokens, and Copilot bills the same. Caudra now counts `claude-sonnet-5-5` and `copilot/claude-sonnet-5.5` at the new price.

### Model picker

- **Favorite models.** In `/model`, press `Ctrl+F` on a model to mark it as a favorite, and again to unmark it. Favorites are listed first, above Recent, and are kept across sessions. `/favorite-next` or `Ctrl+X c` switches to your next favorite. See [Model jobs](/docs/providers/#model-jobs).
- **Recent keeps 10 models.** The Recent section now lists the last 10 models you switched to instead of 4, and leaves out your favorites.
- **Marks on every model row.** A model keeps its row in its provider's section, where a `★` marks a favorite and a `↺` a recent model, so you can see either one while you search.

### Decision engine

- **Reranking features.** Six new experimental decision features use the engine as a reranker, and each is off by default. `memory_search` shows the memory search matches the engine judges relevant, best first, and names the ones it left out. `memory_recall` reminds the agent of up to three notes that may matter for your new message. `memory_duplicates` points out an existing note when a new one may repeat it. `compaction_retention` keeps the tool results that matter to the current goal, todos, and request when a session compacts, instead of only the newest three. `output_pointers` adds up to two lines to a truncated tool result that point at the omitted lines most likely to matter, with the `tool_output` call that reads them. `code_context` moves the most relevant rows of a `code_context` result to the top. Each asks one true or false question per candidate, up to 16 per request, with redacted and bounded candidate text. When the engine fails or times out, each behaves as if it were off. A new `rerank_timeout_ms` deadline (2 seconds) and `thresholds.relevance_floor` (`0.1`) apply to them. See [Reranking](/docs/configuration/#reranking).
- **Tool search judges each candidate.** With `tool_search` enforced, the engine now scores each of the first 16 keyword matches instead of picking one from the whole list. It reorders them and never drops one. On the local evaluation set it got one more query right than the single pick, with no new wrong answers, at about 130 ms more per search.
- **Workflows can rerank.** `rerank(query, candidates)` and `rerank(query, candidates, opts)` return `#{ index, score }` entries, most relevant first. Options are `model`, `timeout_ms`, `threshold`, and `top_k`. Each batch of 16 candidates is one journaled decision, so a resumed run asks only for the rest. A `decide()` or `rerank()` timeout may now reach the larger of `timeout_ms` and `rerank_timeout_ms`. See [Reranking candidates](/docs/workflows/#reranking-candidates).
- **Decision rows in the transcript.** When a decision feature changes what happens, the transcript shows one row that names the feature and what it did, such as `code_context · moved 3 of 12 rows up`, placed after the tool call or at the turn it affected. A row also appears when the engine fails and the default applies, once per feature until the engine answers again, and a dim `shadow` row shows what a feature in shadow mode would have done. Click a row to see the candidates and scores behind it. The rows are saved with the session and return in place when you reopen it. The model never sees them, and `[ui] show_decisions = false` hides them. See [Decision rows](/docs/permissions/#decision-rows-in-the-transcript).
- **Workflow decisions are labelled.** In `/workflow`, a `decide()` call now reads like `decide: ready, route` and a `rerank()` batch like `rerank: 17 candidates · batch 2 of 2`, instead of a bare call number. Labels hold only question IDs and counts, never the state or candidates.
- **Shell duration estimates read plainly.** With `shell_duration` set to `advise` or `enforce`, a shell card's header read like `ETA 1–1s (engine prior; 0 samples)`. It now shows an estimate only for a command expected to take more than a second, in the same units as the timeout beside it, such as `ETA 10s–2m`, and leaves out where the estimate came from. Decision rows still name the source, without a count for an engine estimate.

### Tasks

- **Run a task on a model you name.** Ask for a model, as in "ask opus and sol to review this change", and the main agent starts the task on it, with one task per model when you name several. The `task` tool takes a `model`, either an exact `provider/model` name or a short name such as `opus` or `gpt-6.1-sol` that resolves to the newest match you can use, so `opus` picks `claude-opus-5-5`, and a `thinking` level from `off` to `max`. Both win over a profile's `subagent_model` and `subagent_thinking`, and every result reports the model and level the task ran on. A name that fits several models fails and lists them. `[agent] task_model_choice = false` turns this off. See [Models and reasoning](/docs/commands/#models-and-reasoning).

### MCP

- **The current MCP protocol.** Caudra now speaks MCP `2026-07-28` and still speaks the older protocol that starts with `initialize`, so servers built on either work. It asks each server which versions it supports when it connects. See [Protocol versions](/docs/mcp/#protocol-versions).
- **Servers can ask you questions.** A server can ask for input during a tool call, with a form of typed fields or a link to open, such as a sign-in page. The question form shows it with the server's name, checks each answer against the server's limits, and lets you review before sending. `F3` declines and `Esc` cancels. A link opens in your browser only after you agree, and the form warns about lookalike characters in its host. ACP clients that support MCP elicitation get the question, and print and SDK sessions tell the server they cannot ask. While autopilot is engaged, it may answer a form from the main agent's call, but never a link, a form from Executor, or a form asking for a credential. See [Server questions](/docs/mcp/#server-questions).
- **Long calls report progress.** A server's progress shows beside the card's label, and each report restarts the `timeout` wait, up to ten times `timeout`. Stopping the run tells the server the call was cancelled. A dropped connection resends only requests that are safe to repeat. Other tool calls fail with `connection lost; the call's outcome is unknown`. See [Long calls](/docs/mcp/#long-calls).
- **Tool lists stay current.** When a server announces that its tools, prompts, or resources changed, Caudra lists them again, and the model sees the change on its next request. Long lists are read page by page.
- **Resources and completions.** Mention a server's resource as `@server:uri`, and Caudra reads it when you send the message and adds it the way it adds a mentioned file. Typing `@` lists the resources and templates of running servers beside your files, and a server that offers completions suggests the values a template needs. The model can list and read resources with the new `mcp_resource` tool, which works in plan mode and asks first, with an option to always allow reads from that server. Prompt arguments complete from the server as you type, and a prompt's images and embedded resources now reach the model. See [Resources](/docs/mcp/#resources).
- **Server instructions reach the model.** The instructions each running server gives for its tools are added to the system prompt, up to 4096 bytes per server. See [Server instructions](/docs/mcp/#server-instructions).
- **Executor and Home Assistant are recognised.** Executor gets a card that shows how a run ended, its logs, and what a paused run waits for. An Executor `resume` call that approves a paused run always asks you, even under autopilot, and shows what the run waits for. Add `?elicitation_mode=native` to Executor's URL so it asks you directly. Home Assistant gets cards for intents and for its live context, and a call that controls a `lock`, `alarm_control_panel`, `cover`, or `valve` always asks, even with an allow rule or in Auto. See [Recognised servers](/docs/mcp/#recognised-servers).
- **Read-only tools work in plan mode.** Tools that Executor or Home Assistant mark read-only, such as Home Assistant's `GetLiveContext`, are low risk and can be called in plan mode and by read-only agents. They still ask, and the prompt offers to always allow the tool with any arguments, for the conversation, the project, or all projects. See [MCP tool calls](/docs/permissions/#mcp-tool-calls).
- **MCP cards open whole.** A finished MCP call's card shows its arguments and its whole result rather than the first three rows, and a read-only call's card stays open after newer calls. In the auto view a click folds it to its header. Compact view and `ui.always_collapsed` still fold it. See [Results in the transcript](/docs/mcp/#results-in-the-transcript).
- **MCP cards can show the raw result.** An Executor or Home Assistant card drawn its own way, or a card whose `structuredContent` no text block repeats, ends with a `click for raw` row. The raw view shows the result's JSON indented and coloured, other text as the server wrote it, and the `structuredContent` under its name. Click again to return to the card. See [Results in the transcript](/docs/mcp/#results-in-the-transcript).
- **Server errors show as failed cards.** A result a server marks as an error draws its card as failed, keeps the server's content, and lists any questions asked during the call. The model reads the same error text as before.
- **`/mcp` shows what a server reported.** Select a running server to see its name and version, the protocol version, the features it offers, and the first lines of its instructions. Recognised servers show their kind, and an Executor server in model mode shows how to switch it to native mode. The details area keeps the height of the longest server's details, so moving through the list no longer resizes the picker. See [Runtime toggling](/docs/mcp/#runtime-toggling).
- **A sign-in the server refuses is reported.** `caudra mcp auth` now checks that the server accepts the stored sign-in, and signs in again when it does not. When a fresh sign-in is still refused, as on Home Assistant's `/api/mcp` for a user who is not an administrator, the command fails and `/mcp` shows the server as Refused instead of asking you to sign in again. Home Assistant's `/api/mcp/assist` works for any user. See [OAuth](/docs/mcp/#oauth).
- **Deprecated features are refused.** Caudra never offers roots, sampling, or logging to a server. It refuses roots and sampling requests and writes log messages to `caudra.log` at debug level.

### Interface fixes

- **The wheel scrolls lists.** In `/decisions`, `/automations`, `/workflow`, `/peers` and `/messages`, the wheel over the list now scrolls it instead of moving the selection and loading every row it passes. Click a row or use the arrow keys to select one.
- **Files picked with `Ctrl+S` are mentioned.** The file picker inserted the bare path, so the file was not attached when you sent the message. It now inserts the path as an `@` mention, quoted when it has spaces, like a path chosen from the `@` popup. See [Mention a file with @](/docs/context/#mention-a-file-with).
- **The `Ctrl+X` panel takes the mouse.** Rest the pointer on a chord in the panel the leader lists and it highlights, click it to run it as its key would, or click anywhere else to cancel the chord, as `Esc` does.
- **`minimal` no longer turns reasoning off.** On a model whose lowest level is `none`, such as several OpenAI models and local servers that declare `none`, asking for `minimal` thinking sent `none` and the model did not reason at all. It now sends the shallowest level the model has, usually `low`. Asking for `off` still sends `none`.
- **Copilot no longer sends `none` to a model that always reasons.** On Copilot and Copilot Enterprise, thinking `off` on a model that cannot turn reasoning off, such as `gpt-6.1-sol`, could send `none`, and the request failed with `Unsupported value: 'none' is not supported`. Copilot's model list replaced Caudra's own levels for the model, so a list that offered `none`, or offered no levels at all, decided what was sent. Caudra now keeps its own levels when the list offers none, and never sends `none` to a model its table says always reasons. Such a model runs at its lowest level instead. A `none` thinking setting now means the same as `off` on every model.
- **Custom OpenAI-compatible models without declared levels take no thinking setting.** On an `openai` or `openai-responses` provider in `providers.toml`, a model that declares no `reasoning_options` was offered thinking levels it was never sent, so the footer and task results named a level that did not apply, and an `openai-responses` endpoint was sent the level as typed. Such a model now takes no thinking setting and shows none. Declare its levels with `reasoning_options` to offer them. See [Model defaults](/docs/providers/#model-defaults).
- **Hidden Orca tabs ring when a turn ends.** In an Orca remote workspace, a Caudra tab you were not looking at could show `working` long after its turn ended, because Orca stops reading the status of a hidden tab after a while. Caudra now also rings the bell when a turn ends or a prompt waits while its terminal does not have focus, so Orca marks the tab unread. Turn on **Terminal Bell** in Orca's notification settings for a desktop notice as well. When you return to the tab, Caudra sends its status again, so the tab shows the current state without a key press. `notifications = "off"` stops the bell. See [Orca](/docs/notifications/#orca).
- **Prompts, popups and scrollbars follow the theme.** Some surfaces were drawn in the terminal's own background and text colours instead of the theme's, so a dark theme on a light terminal showed light patches. The affected surfaces were the permission prompt, the `@` and `#` completion lists, the sandbox manager, every scrollbar thumb, and the space beside the permission prompt and the question form over the workbench. They now use the theme's colours and change with it when the theme switches between dark and light.
- **A task's footer names the model it runs on.** The footer of a task's chat could show the main session's model instead of the task's, and the task's card in the transcript then named no model. The footer now shows the model and thinking level the task actually runs on, and the card names that model, including one picked by a profile pin or subagent routing.

### Sessions

- **Open new sessions in build mode.** Set `default_mode = "build"` under `[ui]` to start new terminal UI sessions in build mode instead of plan mode. A resumed session still reopens in the mode it was left in, and `/new` in an idle tab keeps that tab's mode. On a fresh start, Caudra also picks the model you last used in that mode. `--print` and ACP already start in build. See [`ui`](/docs/configuration/#ui).
- **Delete the current session.** In `/sessions`, `Ctrl+D` on the current session now deletes it after the usual confirmation and opens a new session in its place, in the same mode and directory. A session that is still working is refused until it is idle. See [Sessions](/docs/commands/#sessions).
- **Tabs are named after the session.** Caudra sets the terminal window title to the title of the focused session, so most terminals and Orca label the tab with it until you name the tab yourself. Inside Herdr, Caudra renames the tab while it still shows its number, and again when the session title changes, as long as the tab still shows the title Caudra gave it. A tab you named keeps its name. On exit, a terminal with a title stack gets its earlier window title back. Herdr cannot clear a tab name, so the tab keeps the last session title. A plugin that sets the window title keeps it from then on. See [Tab names](/docs/sessions/#tab-names).
- **Forks open in a new Herdr or Orca tab.** Inside a Herdr pane or an Orca terminal, **Fork here** saves the new session and resumes it in a new tab of that host, with a Caudra of its own. The tab takes focus and the session you forked from stays open where it was. Remote, sandbox, and ephemeral sessions still fork into a tab of the current Caudra, and so does a fork the host cannot open. See [Forks in Herdr and Orca](/docs/sessions/#forks-in-herdr-and-orca).
- **Memory writes wait out a busy database.** The `memory` tool failed with `memory journal database operation failed: database is locked` when another Caudra process held the database's write lock for more than 5 seconds, such as while saving a large session. Memory writes, and the background work that builds memory summaries, now keep retrying for up to 30 seconds, like session saves do. A write that times out changes nothing, so it is safe to retry.
- **Moving into a directory ignores its pending reverts.** A move was refused with "Destination session … has a pending restore; resolve it before moving sessions" when any session already in the destination had a pending revert, even an old conversation revert. `/move-session` and `/migrate-sessions` stopped there. `/worktree remove` and other moves that brought this Caudra back to such a directory committed the move and then failed to start, with "UI startup failed; reopen the committed sessions at the destination". Sessions already in the destination no longer affect a move, just as they never stopped a new session from starting there. The rules for the directory a session leaves are unchanged.

### Command line

- **`caudra update` no longer prints its installer.** It names the release it moves to, installs it without asking, and ends with the `caudra rollback` command that returns to your current version. `caudra update --show-script` prints the checksum-verified installer and asks before running it, as before. `-y` now answers only that question, and `--no-color` is still accepted, so existing scripts keep working. See [`caudra update`](/docs/cli/#caudra-update-caudra-rollback).

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
> **After you update, keep `{file:PATH}` out of your config files until every Caudra process that reads them runs Preview 7.** Older versions read it as literal text.

### Providers

- **Copilot reasoning stays whole.** With GPT models on Copilot and Copilot Enterprise, a finished turn split its reasoning into a separate block for each streamed word. Caudra then sent that reasoning back to the model as plain reply text, once in fragments and once in full, and dropped the encrypted reasoning the model needs on later turns. Each reasoning summary is now one block, and later turns send the encrypted reasoning back as they do with OpenAI. Turns saved before the update keep their split blocks.
- **Copilot models compact before Copilot's prompt limit.** Copilot lists two limits for each model: the total window, which also holds the reply, and a smaller prompt limit that it enforces on its own. Caudra used the total, so `copilot-enterprise/gpt-6.1-sol` ran at 400k while Copilot refused any prompt over 272k with "prompt token count of … exceeds the limit of 272000". Caudra now uses the prompt limit as the window, and sessions compact before they reach it.
- **OpenCode models keep their whole prompt budget.** models.dev lists a separate input limit for some models, such as the GPT-5 models on OpenCode. Once the model list loaded, Caudra counted that limit as a total, so it compacted at 80% of it instead of 90%. With background work running, it also refused a prompt past the limit minus the reply allowance, 144k of a 272k budget. These models now run at their full input limit.
- **Set a built-in model's window in `providers.toml`.** A `[[SLUG.models]]` entry or a `[SLUG.model_defaults]` table now sets `context_window` and `max_output_tokens` for a built-in provider such as `anthropic`, ahead of what the provider lists and the built-in model table. Built-in providers ignored both before, although the docs said the Claude window could be set this way. Other model keys on a built-in provider are still ignored, and Caudra now warns about them in `model_defaults` too. See [Model limits on a built-in provider](/docs/providers/#model-limits-on-a-built-in-provider).

### Configuration

- **Values from files.** Beside `{env:NAME}`, a string value in the global `caudra.toml`, `permissions.toml`, `mcp.toml`, `providers.toml`, or `workcell.toml` can now hold `{file:PATH}`. Caudra puts the file's text there, trimmed at both ends, so a secret that a password manager or a deployment tool keeps in a file stays out of the config. A relative path starts at the config file's directory, `~/` is your home directory, and a path can hold `{env:NAME}`, such as `{file:{env:CREDENTIALS_DIRECTORY}/token}`. The file must be a UTF-8 regular file of at most 1 MiB. A missing or unreadable file is an error that names the config file, the line, and the path, never the contents. Project files under `.caudra/` cannot use references, and Caudra reads the file again when it reloads the config file that names it. See [Values from files](/docs/configuration/#values-from-files).

### Terminal

- **Copying works in WSL.** In WSL, Caudra showed "Copied" while the text often stayed in the WSLg clipboard and never reached Windows. Every copy now also asks the terminal to take the text with OSC 52, which Windows Terminal and Orca pass to the Windows clipboard. In Orca, this needs Allow TUI Clipboard Writes (OSC 52), which older Orca releases leave off.

### Plan mode

- **A saved plan goes to the plan menu.** With the question-tool nudge on, a plan-mode reply that offered to implement the saved plan was turned into a question about implementing it. Choosing to implement there could not leave plan mode, so the model refused. Once the plan is saved, the nudge now stays off until your next message, and the plan-mode instructions ask the model to end with a summary instead of asking. Approve the plan from the plan menu as before.

### Sessions

- **Reopening a session another Caudra carried on saves again.** If you left a session with `Ctrl+X n` or `/new`, carried it on in another Caudra, such as an Orca pane, and then reopened it in the first one, every save failed with "session … was modified concurrently: expected write version …, found …". Nothing written after reopening it was saved, and no other Caudra could open the session until you quit the first one. The session on disk was not damaged. Deleting such a session from the session picker failed the same way. Both now work.
- **Sessions compact instead of failing near the limit.** Caudra compacted once a conversation filled 80% of a window the prompt and the reply share, even when the model may reply with more than the other 20%, like `claude-haiku-4-5` with 64k of a 200k window. Between those points, a provider that counts the reply against the window could refuse the request, and with background work running Caudra refused it itself. Compaction now fires early enough to leave room for the reply, up to half the window. `/context` and the status bar show the new point.

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

- Configuration edits apply while Caudra runs.
- Global config files can read environment variables.
- Fewer questions about shell lines that set variables.

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

- Hover tooltips.
- The workbench explorer shows Caudra's own files and folders you choose.
- Narrower permission scopes for commands that take paths.
- xAI sign-in works again.

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

- Project memory is rebuilt around a summarized journal.
- GitHub sign-in for Copilot, and a Copilot Enterprise provider.
- Available updates show in every session.

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

- Installation and recovery are more reliable.
- New opt-in decision features.
- Background shell commands are easier to read.

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
