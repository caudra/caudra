---
title: "ACP (Agent Client Protocol)"
description: "Drive Caudra from your editor, like Zed, over the Agent Client Protocol."
---

Run Caudra inside your editor. `caudra acp` starts an [ACP](https://agentclientprotocol.com/) server over stdio, so any ACP-capable editor (like [Zed](https://zed.dev/)) or desktop app can drive Caudra as its coding agent.

```bash
caudra acp
```

## Zed setup

Add Caudra as a custom agent in Zed's `settings.json`:

```json
"agent_servers": {
  "Caudra": {
    "default_config_options": {
      "model": "deepseek/deepseek-v4-flash"
    },
    "type": "custom",
    "command": "caudra",
    "args": ["acp"],
    "env": {}
  }
}
```

The `model` value is a `provider/model-id` spec, same format as `caudra --model`.

## Launch flags

```bash
caudra acp
caudra acp -m anthropic/claude-sonnet-4-6
caudra --yolo acp
caudra --auto acp
caudra --no-jit acp
```

`-m` / `--model` picks the model new sessions open on. `--yolo` and `--auto` set the starting [permission mode](#config-options) and work on either side of `acp`. `--login` signs in as `caudra auth login` does and exits instead of serving. Other flags go before the subcommand, and [`caudra acp`](/docs/cli/#caudra-acp) lists the ones that also work after it. `--automation` and `--autopilot` are refused with `acp`. `--no-jit` has an effect only when `experimental.lua_plugins` is on.

Providers and permissions come from your normal Caudra config, and accounts from `caudra auth login`. See [providers](/docs/providers/).

## Authentication

Caudra signs in to providers with `caudra auth login`, the same accounts the TUI uses. `initialize` offers one auth method, `caudra-login`, in the form the editor can run:

| Editor advertises | Auth method |
| --- | --- |
| `clientCapabilities.auth.terminal: true` | `type: "terminal"` with `args: ["--login"]`. The editor runs its `caudra acp` command with `--login` added, which opens the provider picker of `caudra auth login`. |
| Anything else | A plain method whose `_meta["terminal-auth"]` holds `command` (the path of the running `caudra`), `args: ["auth", "login"]`, and `label`. Zed reads this form. |

A session needs credentials for the provider of its model. `session/new`, `session/load`, `session/resume`, and `session/fork` check this before they start anything. When the provider has no account, the request fails with `auth_required` (`-32000`), and `message` and `data.details` say which `caudra auth login` command to run. Credentials a provider rejects during a turn fail the prompt the same way. Local providers such as Ollama need no key, so they always pass.

`authenticate` with `methodId: "caudra-login"` reads your accounts again after you signed in. It also resolves the default model again, the way `caudra acp` did at startup, and adds the models of the providers you signed in to. It answers `{}` when the default model now has credentials, and `auth_required` with details when it still has none. Any other method id gets `invalid_params`. Caudra does not offer `logout`. Run `caudra auth logout` in a terminal instead.

When no provider is signed in and neither `-m` nor `default_model` names a model, `caudra acp` still starts, so the editor can connect and offer the sign-in.

## What works

- **Sessions persist.** Load, resume, list, fork, close, and delete sessions. One process serves every session the editor opens. See [Sessions](#sessions).
- **Model, effort, mode, and permissions.** The editor's pickers change them mid-session. See [Config options](#config-options) and [Model catalog](#model-catalog).
- **Slash commands.** `/compact`, your skills, and MCP prompts. See [Slash commands](#slash-commands).
- **Cancellation and steering.** A turn can be cancelled at any point, and an editor can steer a running turn with another prompt. See [Prompts, cancellation, and steering](#prompts-cancellation-and-steering).
- **Live tool calls.** Tool progress streams as it happens, including batched calls, with diffs, structured shell output, usage, and spend. See [Turn updates](#turn-updates).
- **Permissions and questions.** Tool permission prompts and the `question` tool appear in the editor. See [Permissions and questions](#permissions-and-questions).
- **Subagents and background tasks.** The editor sees what subagents do and which shell commands run in the background, in the richest form it supports. See [Subagents and background tasks](#subagents-and-background-tasks).
- **Images and context.** Prompts can include images and editor-attached files.
- **MCP servers.** Servers from `mcp.toml` and the stdio and HTTP servers the editor passes. See [Sessions](#sessions).

## Sessions

ACP session ids are Caudra session ids, the same ones the TUI's session picker and `caudra --resume <id>` use. One `caudra acp` process serves every session the editor opens, and a slow session start never holds up the others. Each session runs its own tools, so sessions in different projects or sandboxes run side by side. One Caudra process can own a session at a time, as [Active sessions](/docs/sessions/#active-sessions) describes, so close a session in the editor before you resume it in a terminal, or fork it.

| Method | What it does |
| --- | --- |
| `session/new` | Opens a session in the given working directory. |
| `session/load` | Opens a stored session and replays the full conversation. It resumes on the model it recorded while your model policy still allows it. Loading a session that is already open replays it again. |
| `session/resume` | Opens a stored session without replaying it. |
| `session/list` | Your local sessions, newest first, 100 per page with a cursor, optionally filtered by directory. Each entry has its title, last-update time, and `_meta.caudra` with `model`, `firstPrompt`, and `lastPrompt`: the first and last prompt you typed, trimmed and cut to 280 characters, without images, file mentions, or Caudra's reminders, or `null` when there is none. |
| `session/fork` | Copies a stored session into a new one that carries the conversation and permission mode, without its conversation approvals. |
| `session/close` | Cancels a running turn, then closes the session. |
| `session/delete` | Deletes a stored session, and closes it first when it is open. |

A prompt, close, or option change naming a session that is not open fails with `-32002` (resource not found), and so does a load, resume, or fork of a session that does not exist. Extra directories an editor asks for are checked but grant nothing: a session reaches only its working directory.

The model-written session name reaches the editor as a `session_info_update`. Loading, resuming, or forking a named session sends its stored title and last-update time right after the response.

MCP servers the editor passes on `session/new` and `session/load` join the servers from `mcp.toml`. Caudra connects to stdio and HTTP servers and drops other transports with a warning. A remote Workcell session refuses editor servers with `invalid_params`. Project MCP startup trust must already be approved through `/mcp` in the TUI. ACP returns an actionable session error instead of silently omitting a parked server.

Plan mode in ACP uses the same state-directory plan files as the TUI (`…/projects/<project-id>/plans/<slug>.md`) rather than the SDK's `./plan.md`. A session keeps one plan across mode switches, so returning to plan revises the same document.

## Config options

Every session offers these ACP config options, always in this order. Editors such as Zed and Orca draw their pickers from them.

| Id | Category | Values |
| --- | --- | --- |
| `model` | `model` | `provider/model-id` specs, grouped by provider |
| `effort` | `thought_level` | The levels the TUI's thinking picker offers for the current model: `off` when the model can stop reasoning, then its effort levels |
| `mode` | `mode` | `build` or `plan`, the same as `session/set_mode` |
| `permissions` | none | `ask`, `auto`, or `yolo`, starting from `--auto`, `--yolo`, your config, or the mode the session saved |
| `fast` | `model_config` | A boolean, offered only for a model that sells fast mode and only to an editor that advertises boolean config options |

`effort` is absent for a model that cannot reason. A model switch keeps the level you asked for and sends the nearest level the new model declares, as the TUI does, so the option shows that level. The level and fast mode are saved with the session and come back when it loads, resumes, or forks.

All configured providers show up in `model`. Providers that list their models over the wire (OpenRouter and friends) are discovered in the background, so the list keeps filling up for a moment after the session starts, one provider at a time.

`session/set_config_option` answers with the full list. Caudra also sends a `config_option_update` whenever the options change: after a change from the editor, when discovery adds models, and when the agent moves the conversation to another provider's model because its own ran low on quota. A mode change also sends `current_mode_update`. An update that would repeat the list the editor already has is not sent. An unknown option id or a value the option does not offer gets an `invalid_params` error that names it.

## Model catalog

An editor can show models before it opens a session. The `initialize` response carries `_meta.caudra.models` and `_meta.caudra.defaultModel`, the `provider/model-id` spec a new session opens on. Each model has these fields:

| Field | Value |
| --- | --- |
| `id` | The `provider/model-id` spec, the value the `model` config option takes |
| `name` | The model id without its provider |
| `provider` | The provider slug |
| `contextWindow` | The context window in tokens |
| `efforts` | The values the `effort` config option offers on this model, empty for a model that cannot reason |
| `fast` | Whether the model sells fast mode |

The list holds the models known when the editor initializes. `_caudra/models/list` answers with the same `models` and `defaultModel` at any time after `initialize`, with no session. Caudra also sends the `_caudra/models/changed` notification with the same params whenever discovery or a sign-in adds models. An editor that does not know these extension methods ignores them.

## Slash commands

The editor gets an `available_commands_update` after it opens a session, and again when the list changes, for example once MCP servers finish connecting. A prompt that starts with `/name` runs that command when the session offers it. Any other text, including an unknown `/name`, goes to the model as typed.

| Command | What it does |
| --- | --- |
| `compact` | Summarizes and compacts the conversation, as the TUI's `/compact` does. The prompt ends with `end_turn` once compaction completes. |
| A skill's name | Loads that skill into the turn, then sends your prompt. Text after the name is optional instructions. |
| An MCP prompt's `server:prompt` name | Expands the prompt, as the TUI does. Words after the name fill its arguments in order, the last taking the rest of the line. A missing required argument gets an `invalid_params` error. |

When a skill and an MCP prompt share a name, the skill wins, and `compact` wins over both.

## Prompts, cancellation, and steering

Every prompt has an id. Caudra takes it from the request's `_meta.promptId` when that is a string and generates one otherwise. Every `session/update` the turn sends carries it in `_meta.caudra.promptId`. The prompt response carries `_meta.caudra` with `promptId`, `durationMs`, and `usage`, the turn's token usage in full. A turn Caudra starts on its own, such as the one that reports a finished background task, answers no prompt, and its updates carry no `promptId`. A prompt sent while it runs waits for it to end.

A second prompt to a busy session is refused with `invalid_request` (`-32600`), unless its turn was cancelled or the prompt steers.

`session/cancel` ends the running turn with stop reason `cancelled`, and the prompt is answered exactly once:

- Permission prompts and questions still open in the editor are withdrawn with `$/cancel_request` and count as denied.
- Caudra waits up to 5 seconds for the agent to stop. If it has not stopped by then, calls still running are reported `failed` and the prompt is answered `cancelled` anyway.
- A cancel that arrives before the turn starts, or while it is settling, still answers `cancelled`.
- A prompt sent after `session/cancel`, before the cancelled prompt is answered, waits for that answer and then starts the next turn. Cancelling and prompting again at once is how editors such as Orca steer. Another `session/cancel` answers the waiting prompt `cancelled` too.

`$/cancel_request` naming a running `session/prompt` does the same as `session/cancel`. Naming a request still queued behind a session that is starting or closing removes it, and it fails with `request_cancelled` (`-32800`). Any other request runs to completion.

Caudra advertises `agentCapabilities._meta.caudra.steering: true`. A prompt sent with `_meta.caudra.delivery: "steer"` while a turn runs joins that turn instead of being refused:

- The agent reads it at its next step, as a message typed into the TUI while a turn runs does.
- A steer the turn took is answered when that turn ends, with the same stop reason and `_meta.caudra.steeredInto` naming the turn's prompt id.
- A steer the turn ended without taking starts the next turn, and is answered when that turn ends.
- When the turn is cancelled or fails, its steers end the same way.
- `/compact` cannot steer, and a steer sent after a cancel is refused.

A steer with no running turn is an ordinary prompt.

## Turn updates

- **Message ids.** Every message and thought chunk carries a `messageId`, and so does every prompt chunk a load replays. A message keeps its id when the session is loaded later. Each run of thinking in a message gets `<message id>:thought:<n>`.
- **Tool calls.** Every tool call names its tool in `_meta.caudra.toolName`. A shell call's `rawOutput` is `{stdout, stderr, exitCode}`, beside the output as text. A write that replaces a file sends a diff of the whole old and new file. A call still running when its turn ends or is cancelled is reported `failed` with the text `Cancelled` before the prompt is answered.
- **Usage.** A `usage_update` follows every model response, and every load, resume, or fork. It carries the context gauge and the session's spend so far, including side calls such as titles and compaction. After a load, the gauge is estimated from the stored conversation. The prompt response repeats the turn's usage in `usage` and in `_meta`.
- **Compaction and retries.** An editor that advertises `session.compaction` sees a `compaction_update` when compaction starts and when it completes, fails, or is cancelled. An editor that advertises `session.notices` gets a warning `notice` each time a provider request is retried. Other editors get neither. A loaded session does not replay past compactions.
- **Stop reasons.** A provider refusal or content filter ends the prompt with stop reason `refusal`.

## Permissions and questions

Tool permission prompts arrive as `session/request_permission`. "Allow exact call for conversation" stores only the reviewed input and resources. Broad tool authority is not exposed through ACP.

The `question` tool becomes a native form in an editor that supports ACP form elicitation. An editor without it that sets `clientCapabilities._meta["caudra/questionsViaPermission"]: true` gets a question with a single choice list as a `session/request_permission` instead, with the tool call id `interaction_<call id>` and one option per answer, `answer_0`, `answer_1`, and so on. The `openmausbot` client gets this too, matched by its `clientInfo.name`, because it used this convention before the flag existed. If the client supports neither, the tool is dropped and the model asks in plain text. URL-mode MCP elicitations send `elicitation/complete` when they finish.

## Subagents and background tasks

Subagents and background shell commands work as they do in the TUI, so the `auto` and `async` [execution policies](/docs/sessions/#background-tasks) can move them to the background. Workflows are not available over ACP. Cancelling a turn cancels its subagents and stops the session's background work. Closing a session stops both too.

Caudra picks one of three ways to show this work, from the editor's `initialize` request:

| Editor advertises | What it sees |
| --- | --- |
| `clientCapabilities.subagents` | ACP subagent sessions |
| `clientCapabilities._meta["caudra/childWork"]: true` | The subagents' tool calls on the parent, plus `_caudra/*` notifications |
| Neither | The subagents' tool calls on the parent |

Caudra advertises `agentCapabilities._meta.caudra.childWork: true`. An editor that advertises both gets subagent sessions, and the same work is never shown twice.

**Subagent sessions.** Each subagent gets its own session, with the id `<parent session id>/<subagent id>`. Before anything from the subagent arrives, the parent session gets a `subagent_update` naming the child session, with the subagent's label as `title`, its prompt as `description`, the state `running`, and `capabilities.cancel`. Then the subagent's messages, thoughts, and tool calls stream on the child session. When it ends, another `subagent_update` moves it to `idle` with stop reason `end_turn`, `cancelled`, or `error`. `session/cancel` naming the child session cancels only that subagent.

**Tool calls on the parent.** A subagent's tool calls appear in the parent session with the id `<subagent id>:<tool call id>`. The title starts with the subagent's label, and `_meta.caudra.subagentId` names the subagent. Its messages and thoughts are not shown.

**`_caudra/*` notifications.** These are extension notifications and requests on the parent session, sent beside the tool calls. A frame is at most 32 KiB. Labels, errors, commands, and directories are cut to 1 KiB. Output is cut to fit the frame. A field without a value is left out.

| Method | Kind | Params |
| --- | --- | --- |
| `_caudra/subagent/spawned` | Notification | `sessionId`, `subagentId`, `parentPromptId`, `description` (the subagent's label), `model` |
| `_caudra/subagent/progress` | Notification | `sessionId`, `subagentId`, `durationMs`, `turnCount`, `toolCallCount`, `tokensUsed`, `contextUsagePct`, `activity` |
| `_caudra/subagent/finished` | Notification | `sessionId`, `subagentId`, `status` (`completed`, `failed`, or `cancelled`), `error`, `durationMs`, `toolCalls`, `turns`, `tokensUsed`, `output` |
| `_caudra/task/backgrounded` | Notification | `sessionId`, `toolCallId`, `taskId`, `command`, `cwd` |
| `_caudra/task/completed` | Notification | `sessionId`, `taskId`, `state` (such as `succeeded`, `failed`, or `cancelled`), `exitCode`, `signal`, `output`, `truncated`, `outputTotalBytes` |
| `_caudra/subagent/cancel` | Request | `sessionId`, `subagentId`. Cancels that subagent and answers `{"outcome": "stopped"}`. |
| `_caudra/task/kill` | Request | `sessionId`, `taskId`. Stops that background shell command and answers `{"outcome": "stopped"}` once it has stopped. |

`parentPromptId` is the id of the prompt whose turn spawned the subagent. `progress` is sent only when a count or the activity changed, and at most every 2 seconds per subagent. `finished` carries the final answer as `output`, or the partial answer of a subagent that failed or was cancelled. `truncated` is `true` when `output` holds less than the `outputTotalBytes` the command wrote. A cancel or kill naming a subagent or task that already ended answers `{"outcome": "not_running", "status": ...}` instead, with the `status` its `finished` notification carried or the `state` of its `completed` one. A session remembers its last 256 ended subagents and every background command it ran. A cancel or kill naming a subagent or task the session never ran, or none at all, gets an `invalid_params` (`-32602`) error. `session/cancel` naming the child session of a subagent that already ended does nothing.

## Errors

A failed request carries its text in both `message` and `data.details`, so an editor can show what went wrong instead of a bare "Internal error".

| Code | When |
| --- | --- |
| `-32000` (`auth_required`) | The model's provider has no credentials, or rejected them. See [Authentication](#authentication). |
| `-32002` (resource not found) | The request names a session that is not open, or a stored session that does not exist. |
| `-32600` (invalid request) | A request before `initialize`, a second prompt to a busy session, or a request line over the stdin limit. |
| `-32601` (method not found) | An unknown method. |
| `-32602` (invalid params) | Malformed params, an unknown config option or value, or a subagent or task that is not running. |
| `-32603` (internal error) | Any other failure. A rate limit that outlasted its retries adds `data.kind: "rate_limit"`. |
| `-32800` (request cancelled) | A queued request that `$/cancel_request` removed. |

A line on stdin that is not JSON is skipped. A line over 64 MiB is skipped too, and a request that names its `id` within the first 64 KiB of that line is answered with `invalid_request`.

## Embedding Caudra in a desktop client

A desktop app or orchestrator such as [Orca](https://github.com/stablyai/orca) runs Caudra the way an editor does. This is what such a host needs to know.

- **Launch.** Run `caudra acp` with the session's working directory in `session/new`. Put flags before the subcommand: `caudra --yolo acp` for full access, `caudra --auto acp` for auto mode, and `caudra -m provider/model-id acp` for the starting model. Caudra speaks protocol version 1 and needs no client file system or terminal capabilities, because it runs its own tools.
- **Sign-in.** Run `caudra auth login`, or `caudra acp --login`, in a terminal for the person. After it exits, call `authenticate` with `caudra-login` and retry the session request. Session requests fail with `-32000` and `data.details` while the model's provider has no account. See [Authentication](#authentication).
- **Models without a session.** Read `_meta.caudra.models` and `_meta.caudra.defaultModel` from `initialize`, or call `_caudra/models/list`, and listen for `_caudra/models/changed`. Each model lists its context window and effort levels. See [Model catalog](#model-catalog).
- **Pickers.** Find the options by category: `model`, `thought_level` for effort, and `mode`. The `permissions` option has no category. See [Config options](#config-options).
- **Turn identity.** Send your own id in `session/prompt` `_meta.promptId`. Caudra echoes it in `_meta.caudra.promptId` on every update of the turn and in the response. See [Prompts, cancellation, and steering](#prompts-cancellation-and-steering).
- **Steering.** Cancel and prompt again at once, and the new prompt starts as soon as the cancelled turn ends. A host that wants the message to join the running turn instead sends it with `_meta.caudra.delivery: "steer"`.
- **Child work.** Advertise `clientCapabilities._meta["caudra/childWork"]: true` for `_caudra/*` subagent and background task notifications, with `_caudra/subagent/cancel` and `_caudra/task/kill` to stop one. Advertise `clientCapabilities.subagents` for ACP subagent sessions instead. See [Subagents and background tasks](#subagents-and-background-tasks).
- **Questions.** Advertise form elicitation, or set `clientCapabilities._meta["caudra/questionsViaPermission"]: true` and claim permission requests whose tool call id starts with `interaction_`, so a full-access host does not answer them on its own. See [Permissions and questions](#permissions-and-questions).
- **History.** `session/list` lists the sessions, and `caudra storage sessions --json` lists them without a running server, with each session's `model`, `first_prompt`, and `last_prompt`. To continue a chat in a terminal, close it with `session/close` and run `caudra --resume <id>` in its directory, or `session/fork` it and resume the copy while the chat stays open.
