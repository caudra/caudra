---
title: "MCP (Model Context Protocol)"
description: "External tool servers over stdio or HTTP."
---

Caudra connects to external tool servers over MCP. Both **stdio** and **HTTP** transports are supported.

An external stdio server is an unsandboxed local process. Per-tool permissions control MCP calls after startup. They cannot confine the server process itself.

## Configuration

Add servers under `[mcp.*]` in your MCP config:

- **Global**: `~/.config/caudra/mcp.toml`
- **Project**: `.caudra/mcp.toml`. A project server replaces a global server with the same name.

The TUI applies an edit to either file while it runs. It starts new servers, including the first one when Caudra started with none. It stops removed or disabled ones, and restarts a server whose definition changed. Servers you did not touch keep their connection. A file that fails to load keeps the servers it last loaded, and the error shows in `/mcp`. A changed project server that needs review waits in `/mcp`, and a `[review MCP]` chip in the status bar opens it. The model sees the new tools in its next [server list](#the-server-list), see [Add an MCP server](/docs/configuration/#add-an-mcp-server).

### Stdio

```toml
[mcp.filesystem]
command = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.github]
command = ["gh", "mcp-server"]
environment = { GITHUB_PERSONAL_ACCESS_TOKEN = "{env:GITHUB_TOKEN}" }
timeout = 10000
enabled = false
```

### HTTP

```toml
[mcp.analytics]
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer {env:ANALYTICS_TOKEN}" }
```

In the global `mcp.toml`, `{env:NAME}` takes a value from your environment or the global `.env`, and `{file:PATH}` takes one from a file, so the token stays out of `mcp.toml`. A project `mcp.toml` cannot read the environment or files, and Caudra loads none of its servers while it holds a reference. See [Environment variables](/docs/configuration/#environment-variables) and [Values from files](/docs/configuration/#values-from-files).

Caudra gets an OAuth client from most servers on its own (see [OAuth](#oauth)). For a server it cannot, give Caudra a static client:

```toml
[mcp.acme]
url = "https://mcp.acme.example.com/mcp"
oauth = { client_id = "acme-client", client_secret = "s3cret", callback_port = 3118, callback_path = "/callback", callback_hostname = "localhost" }
```

### All options

<!-- caudra-docgen:mcp-server-fields -->

| Field | Type | Default | Min | Max | Description |
|-------|------|---------|-----|-----|-------------|
| `enabled` | bool | `true` | - | - | Start the server. `/mcp` sets this key when it turns a server on or off |
| `timeout` | integer | `30000` | 1 | 300000 | Milliseconds to wait for each response from the server |
| `command` | string[] | required | - | - | Stdio servers: the program and its arguments. It must not be empty. When `url` is also set, `command` wins |
| `environment` | table | `{}` | - | - | Stdio servers: environment variables for the server process, such as `{ GITHUB_TOKEN = "{env:GITHUB_TOKEN}" }`. In the global file, `{env:NAME}` reads a value from your environment and `{file:PATH}` reads a file. Other values are stored as plain text |
| `url` | string | required | - | - | HTTP servers: the server URL. It must start with `http://` or `https://` |
| `headers` | table | `{}` | - | - | HTTP servers: headers sent with every request, such as `{ Authorization = "Bearer {env:GITHUB_TOKEN}" }`. In the global file, `{env:NAME}` reads a value from your environment and `{file:PATH}` reads a file. Other values are stored as plain text |
| `oauth` | table | unset | - | - | HTTP servers: a static OAuth client, for a server Caudra cannot get a client from on its own |

`caudra config example mcp` prints every `mcp.toml` key with its default, all commented out. [Reference configs](/docs/reference-configs/#mcp-toml) shows the same text.

<!-- /caudra-docgen:mcp-server-fields -->

Set `command` for stdio, `url` for HTTP. Pick one.

The file can start with `version = 1`, and a file without it counts as version 1. When the version is newer than this build reads, no server in the file starts and Caudra shows the error. See [Config file versions](/docs/configuration/#config-file-versions).

Earlier versions read a top-level `defer_tools` key and a per-server `always_load` key. MCP tools now load nothing up front (see [Code mode](#code-mode)), so a file that still sets either key fails to load until you delete it.

## Code mode

Every tool definition in a request costs tokens on every request, and a server can ship more than 100 tools when a task needs three. Changing the definitions costs more: providers cache a request from its start, so a tool array that changes when a server connects or a tool loads makes the provider read the whole conversation again at full price.

So the model never sees an MCP tool definition. It gets one fixed tool, `mcp`, and reaches your servers by writing Python:

```python
issues = await github.list_issues(owner="caudra", repo="caudra")
[issue["title"] for issue in issues if issue["comments"] > 10]
```

`mcp` is declared while at least one server is enabled in `mcp.toml`, whether it has connected or not. A server that connects, reconnects, fails, or changes its tools never changes the tool definitions or the system prompt, and plan mode keeps `mcp` too. See [The prompt cache](/docs/configuration/#the-prompt-cache).

A snippet can filter, join, and summarise results before the model reads anything. Only what it prints and the value of its last expression come back, so a list of 500 issues can come back as three lines.

### The server list

The model learns what it can call from a `# MCP servers` [reminder](/docs/context/#what-caudra-writes-on-your-behalf) in the conversation. [Add an MCP server](/docs/configuration/#add-an-mcp-server) shows one.

Each server has a header line with its state: connected with its tool count, `connecting`, `awaiting trust`, `needs login`, or `failed` with the error. The server's [instructions](#server-instructions) follow its header. Then come its tools, one line each, as a Python signature with the first sentence of the tool's description.

The list aims to stay within about 2,000 tokens. Every server keeps its header and instructions, and the tool lines share the rest. Servers take turns adding their cheapest remaining tool, so one large server cannot push the others out. The list says whether it is complete. When it is not, a header reads like `showing 40 of 117 tools`, and the model finds the rest with `search()`.

Caudra sends the list again only when its text changes, such as when a server connects or its tools change, and after compaction. When no server is enabled any more, a short reminder tells the model to disregard the list.

Each agent gets its own list. Tools that `agent.disabled_tools` or a profile turns off are left out. A [read-only agent](/docs/permissions/#read-only-agents) sees only the read-only tools of [recognised servers](#recognised-servers). In plan mode the main agent keeps its whole list, and a call that plan mode does not allow is refused when it runs.

A [system prompt profile](/docs/system-prompts/#choose-tool-availability) can make an MCP tool `lazy`, which leaves it out of the list but lets `search()` find it. `disabled` refuses it.

### Calling tools

Each server is a Python namespace, and each of its tools is an async function of that namespace. Call a tool with keyword arguments, or with one dict of them, and `await` the result. Run independent calls together with `asyncio.gather`:

```python
import asyncio

issue, pulls = await asyncio.gather(
    github.get_issue(owner="caudra", repo="caudra", issue_number=42),
    github.list_pull_requests(owner="caudra", repo="caudra"),
)
```

Up to 8 calls run at once, and one run makes at most 100. Four functions sit beside the namespaces:

| Function | What it returns |
|---|---|
| `search(query, server=None, limit=10, offset=0)` | The connected tools that match `query`, best first, with their exact signatures and full descriptions. It finds tools the list leaves out. `limit` goes up to 50 |
| `servers()` | Every enabled server, with its state, tool count, and whether it offers resources |
| `await resources(server)` | The [resources](#resources) and resource templates a server offers |
| `await read_resource(server, uri)` | The contents of one resource |

Snippets run in a Python subset on the bundled Monty worker. Top-level `await` works. There is no filesystem, network, subprocess, class inheritance, or third-party package. `asyncio`, `json`, `re`, `math`, `datetime`, `collections`, and `itertools` are among the standard modules it offers.

### Results and errors

A tool returns its `structuredContent` when it has one. Otherwise it returns its text, parsed when the text is a JSON object or array, and a list when there are several content blocks. An image or audio block comes back as a dict with its type, MIME type, and size. The model never receives the image or audio data itself.

A failed call raises `RuntimeError`: the server reported an error, the server is not connected, or the result is larger than 8 MiB. A refused call raises `PermissionError`: you declined it, plan mode or a read-only agent does not allow it, or the tool is disabled. A snippet can catch either and carry on. A misspelt tool name raises with the closest name and a hint to search, and a missing argument, or one the tool does not take, raises `TypeError` that names it.

Before a snippet runs, it is checked against the signatures of the tools it calls, so a misspelt tool or an argument of the wrong type stops it before any call is made, with the closest name and the line. Later runs of the same agent are checked too, but not the variables they take from earlier runs, because those runs alone knew their types.

Some slips have only one possible meaning, so Caudra fixes them before the snippet runs. It imports a standard module the snippet uses without importing it, such as `asyncio` in `asyncio.gather`. It drops `await` before `search()` and `servers()`, removes a Markdown fence around the whole snippet, and drops `return` from a snippet's last line, since a snippet ends on its value. `asyncio.gather(..., return_exceptions=True)` works: each failed call settles as its exception while the others still answer. Each fix keeps the snippet's line numbers, so errors still point at the line the model wrote. The card lists the fixes, and the model reads in the result what it did wrong, so it can avoid the slip next time. The conversation keeps the snippet as the model wrote it.

Code that is not valid Python never runs. The error says what kind of code a snippet is: module code, so `return` belongs only inside a function, and Python, so JavaScript such as `const` or `Promise.all` is named as such.

Calls that ran before a failure keep their effects. When a run fails, the model reads why, followed by every server call the run made and how each ended, so it can check before it repeats one. The list names each call as the snippet wrote it, such as `my_server.get_sum`. Stopping the turn cancels the run and the calls it has in flight.

### Decisions and reranking

With the [decision engine](/docs/configuration/#decisions) on and a base URL set, snippets get two more functions, `decide()` and `rerank()`. They are helpers for what a snippet's servers return, not tools of their own: a snippet calls a server, then judges, filters, or sorts the result before any of it reaches the model, or branches on a judgement without another round trip. The model's list of servers tells it to use them this way:

```python
issues = await github.list_issues(owner="caudra", repo="caudra", state="open")
titles = [issue["title"] for issue in issues]
ranked = await rerank("crashes when saving a file", titles, threshold=0.5, top_k=10)
[issues[entry["index"]]["number"] for entry in ranked]
```

| Function | What it returns |
|---|---|
| `await decide(input, questions, model=None, timeout_ms=None)` | `{"model", "answers", "by_name"}`: typed answers about `input` |
| `await rerank(query, candidates, threshold=None, top_k=None, model=None, timeout_ms=None)` | `[{"index", "score"}]`: the candidates relevant to `query`, best first |

`decide()` takes the same questions as a workflow's [`decide()`](/docs/workflows/#typed-decisions): a list of `predicate`, `choice`, and `score` questions in the shape of OpenAI Decisions, each with a `name`. `input` is the state the questions are about, and it must stay within 1,500 bytes as JSON. Read an answer by its question's name, as in `result["by_name"]["urgent"]["probability"]`.

`rerank()` judges up to 256 candidates, each a string or a small dict. `index` is a candidate's position in `candidates`, and `score` is the probability that it is relevant. `threshold` drops candidates below it, and `top_k` keeps at most that many. Candidates go out 16 to a request, with up to 4 requests at once. Each request waits up to [`rerank_timeout_ms`](/docs/configuration/#decisions), like Caudra's own reranking, unless the call passes a shorter `timeout_ms`. A `decide()` call waits up to `timeout_ms` from the configuration. An empty list returns `[]` without a request.

The model learns about both functions from the `# MCP servers` reminder, which lists them only while the engine is on. Without the engine they do not exist, and a snippet that calls one raises `NameError`. The `mcp` tool still appears only while a server is enabled.

A call needs no approval and works in plan mode, since it only reads. A malformed question or argument raises `TypeError` or `ValueError` before anything is sent. An unreachable engine, a timeout, a refusal, or an invalid answer raises `RuntimeError`, and one failed request fails the whole `rerank()`. The opt-in [decision log](/docs/permissions/#decision-engine-advice) records each call under the feature `mcp`. Answers are data for the snippet: they never grant a permission.

The card shows each call as a row. A `decide()` row lists its verdicts, such as `severity high 82.4% · parser_bug yes 91.0%`, and a `rerank()` row says how many candidates it kept and names the best one. Both rows always show in full: a `decide()` row shows each question with a bar for every option. The likeliest option is highlighted, and a winner below 60% is marked uncertain alongside the runner-up. A `rerank()` row shows the candidates it kept, best first, each with its score and the label it was given. Click the row's footer to switch to the JSON the snippet received, and click it again to go back. Selecting any part of either view copies that JSON whole.

### Variables between runs

Variables that a run defines are still there in the next run of the same agent. The main agent and each subagent keep their own. They live in memory, up to 16 MiB. Past that they are dropped and the next run starts empty. They do not survive a restart of Caudra, and a resumed, rewound, or forked conversation starts without them. The model can pass `reset: true` to start a run empty. When variables are lost, the result says so.

### Where snippets run

Snippets always run on this machine, because your MCP servers are clients of this Caudra process, even when the session's other tools run on a [remote Workcell](/docs/remote-workspaces/). A build without the bundled worker returns an error that says so. With embedded Workcell, Caudra also warns at startup that `python_execution` and `mcp` are unavailable. When a server is configured, the TUI shows the same reason as it starts, and `/mcp` shows it above the server list. `WORKCELL_MCP_CODE_WORKER` can point Caudra at a worker binary.

> **Note**
>
> **Snippets run in a worker process of their own, not inside Caudra.** The worker has no filesystem, network, subprocess, or environment access. A snippet reaches the outside only through the MCP calls and helpers Caudra answers, and each call passes the same permission check as before. A snippet that crashes the worker ends only that process, and Caudra starts a new one. This is process isolation, not an operating-system sandbox: the worker runs as your user.

[`python_execution`](/docs/tools/#python_execution) is a separate tool. It runs isolated, needs no approval by default, can run on a remote Workcell, and cannot reach MCP servers.

A run that starts while servers are still making their first connection waits for them, so every server that comes up is in reach.

### Servers with a code mode of their own

Some servers wrap their tools in a code tool of their own. Inside `mcp`, the model would then write Python that writes code for the server. Ask such a server for its plain tools when it offers them:

- **Cloudflare**: add `?codemode=false` to the URL of Cloudflare's MCP server at `mcp.cloudflare.com`.
- **Executor**: add `?mode=passthrough` to get its search and invoke tools (see [Executor](#executor)).

Caudra does not change a server's URL for you.

### Sessions from earlier versions

A conversation from before code mode resumes and sends as before. When the model calls an MCP tool by its old `server__tool` name, the call fails with an error that points it to `mcp`.

## Naming and namespacing

Server names are ASCII letters, digits, and hyphens, with no dots, and cannot be the name of a built-in tool. Permission rules, `agent.disabled_tools`, and profiles name a tool as `server.tool`, with the server's configured name and the tool's own MCP name, such as `github.list_issues`.

In `mcp`, a server is a Python namespace and its tools are its functions. Each character other than an ASCII letter, digit, or `_` becomes `_`, so the `my-server` server is `my_server`, and a name that starts with a digit gets a leading `_`. A name that is a Python keyword gets a trailing `_`, such as `class_`. A namespace also gets a trailing `_` when it would hide a Python builtin, a standard module, or one of the four functions, such as `json_` for a server named `json`. When two names come out the same, the later one in name order gets `_2`, then `_3`, so names stay the same whatever order servers connect in. The server list and `search()` always show the names to call.

See [Permissions](/docs/permissions/#mcp-tool-calls) for MCP call review and remembered decisions.

Tool-call approval binds to the configured server authority, the discovered tool contract, and one transport snapshot. Reconnects and contract changes cannot inherit an approval by reusing the same display name.

Generic MCP approvals are exact-input by default. The permission review shows the complete validated JSON rather than a truncated preview.

## Results in the transcript

An `mcp` card shows the snippet, numbered and highlighted as Python. Below it comes a row for each server call the snippet made, labelled `server › tool` with its status, then what the snippet printed, the value it returned, and why it stopped when it did not finish. The rows fill in while the snippet runs. A row uses the names permission prompts and rules use, so the snippet's `my_server.get_sum` call is the `my-server › get-sum` row.

Open a call's row to see that call's own card, which the rest of this section describes. A call's card keeps up to 64 KiB of the result, and the snippet receives all of it.

A call's card is labelled `server › tool`, with short arguments beside the label. The header also shows the size of the result and an estimate of the tokens the model reads from it, such as `JSON · 128 lines · ~3.2k tokens`.

When a call runs code, the card shows that code above the result, numbered and highlighted like a `python_execution` card. Caudra takes the string under `code`, `script`, `source`, `program`, `sql`, `command`, or `cmd`, or else the longest multi-line string. The key or the text decides the language. Below the code come the remaining arguments. A multi-line string among them is shown as plain text under its name.

An MCP card opens whole: the arguments and all of the result, with no row budget, read-only or not, and it stays open after newer calls. In the auto `/view`, a click folds it to its header and the next opens it whole. The compact view and `ui.always_collapsed` still fold it to one row.

Each block of the result is drawn by its type. A text block that is a whole JSON object or array is indented and coloured. Other text is rendered as markdown. Images, audio, and resources become labelled rows with their type and size. What the snippet receives does not depend on what the card shows, and the model never receives image or audio data.

A card can also show the result as raw data when that shows something the card does not. That is a card a [recognised server](#recognised-servers) draws its own way, and a card whose `structuredContent` no text block repeats. Such a card ends with a `click for raw` row. The raw view shows each JSON text block indented and coloured and any other text exactly as the server wrote it, followed by the `structuredContent` under its name. Click `click for the card` to switch back. Changing the `/view` keeps the choice.

When the server gives a tool a title, the label shows the title in place of the tool name. While a call runs, the progress the server reports shows beside the label, such as `3/10 · indexing`. A result the server marks as an error draws the card as failed, and the call raises `RuntimeError` with the server's error text.

A card also lists the questions the server asked during the call (see [Server questions](#server-questions)), with each answer and who gave it. When a structured result does not match the output schema the tool declared, the card says so. The call still succeeds, and the log records the mismatch.

[Executor](#executor) and [Home Assistant](#home-assistant) results get cards of their own.

### Long calls

`timeout` is how long Caudra waits without hearing from the server. Each progress report restarts the wait, up to ten times `timeout` for the whole call. The wait pauses while a server question waits for your answer.

Stopping the run cancels the call, and Caudra tells the server. When the connection drops during a call, Caudra sends a list or read request once more, and the same goes for a tool the server marks read-only or idempotent. Any other tool call fails with `connection lost; the call's outcome is unknown`, because sending it again could repeat its effect.

When a server announces that its tools, prompts, or resources changed, Caudra lists them again, and the model sees the change in its next [server list](#the-server-list). Lists are read page by page, up to 64 pages each. A tool whose input schema points to a schema in another document with `$ref` is left out, and the log says why.

## Server questions

A server can ask you for input in the middle of a tool call. The MCP spec calls this elicitation. Caudra shows the request in the question form, headed by the server's name and its message.

- **Forms** ask for typed fields: text with length limits or an email, URI, date, or date-time format, numbers in a range, yes or no, one choice, or several choices. Defaults are filled in, and optional fields can be skipped. Caudra checks each answer against the server's limits before it sends anything, and you review the answers before they go.
- **Links** ask you to open an address, for example to sign in to another service. The form shows the full address with its host highlighted, and it warns when the host uses lookalike characters. Caudra opens the address in your browser only after you choose `Open in browser`, and it never fetches the address itself.

Submitting accepts the request, `F3` declines it, and `Esc` dismisses it, which tells the server you cancelled. Stopping the run cancels the call too. When the server withdraws a question, or the call ends first, the form closes.

| Session | Who answers |
|---|---|
| TUI | You, including questions from a subagent's call |
| ACP | The client, when it supports form or URL elicitation. Otherwise Caudra declines |
| Print and SDK | Nobody. Caudra tells the server it cannot ask, and declines a question from a server on the older protocol |

While [autopilot](/docs/autopilot/) is engaged, it may answer a form for you. It answers only forms from the main agent's calls. It never answers a link, a form from [Executor](#executor), a form with no fields, or a form with a field that looks like a credential, such as a password, token, or API key. An answer that breaks the server's limits is never sent, and the form waits for you. You can always answer first.

## Server instructions

A server can describe how its tools should be used. Caudra puts each server's instructions under its header in the [server list](#the-server-list), and cuts each server's text at 4096 bytes. A server that connects later, or whose instructions change, updates the list, and the system prompt stays as it was.

## Recognised servers

Caudra recognises some servers by the name they report when they connect, and treats them with more care. A recognised server's tools that the server marks read-only work in plan mode and in [read-only agents](/docs/permissions/#read-only-agents). They still ask in Ask mode, at low risk, and the prompt offers to always allow the tool with any arguments. See [MCP tool calls](/docs/permissions/#mcp-tool-calls).

Any server could report one of these names. A server that does gains only its read-only tools in plan mode, and they still go through your rules.

### Executor

Executor runs code that calls your connected services. Its card shows how a run ended: completed with its result, an error, or waiting for approval with what the run is waiting for. Logs are dimmed and folded after eight lines, and the code sits above the result as on other code cards.

Some calls in a run need approval first, and Executor asks for it in one of two ways:

- **Native mode** (recommended). Add `?elicitation_mode=native` to the server URL. Executor asks you directly through a [server question](#server-questions). Autopilot never answers these questions.
- **Model mode**, the default. Executor pauses the run and tells the model, which approves it with the `resume` tool. A `resume` call that accepts always asks you, even with an allow rule, in Auto mode, and under autopilot. The prompt shows what the paused run is waiting for. Declining or cancelling follows your normal rules.

```toml
[mcp.executor]
url = "https://mcp.executor.sh/mcp?elicitation_mode=native"
```

`/mcp` shows the same advice for an Executor server in model mode.

Executor's own tool takes code, so through [code mode](#code-mode) the model writes Python that sends code to Executor. Add `?mode=passthrough` to the URL to get four plain tools in its place:

| Tool | What it does |
|---|---|
| `integrations` | Lists your connected accounts |
| `skills` | Lists or reads Executor's guides |
| `search` | Finds a service's tools, with their IDs and input schemas |
| `invoke` | Calls one tool by its ID |

The model can then call them from Python like any other server's tools. Caudra asks before an `invoke` as it does for any MCP tool call. `/mcp` suggests this option for an Executor server whose URL lacks it.

```toml
[mcp.executor]
url = "https://mcp.executor.sh/mcp?mode=passthrough"
```

### Home Assistant

Home Assistant serves MCP on two endpoints:

| Endpoint | Tools | Account |
|---|---|---|
| `/api/mcp/assist` | The Assist API: intents and live context | Any user |
| `/api/mcp` | The LLM APIs chosen in the integration | Administrator, when the integration requires one |

```toml
[mcp.home]
url = "http://homeassistant.local:8123/api/mcp/assist"
```

An intent's card shows Home Assistant's reply, the devices it acted on, and the ones that failed. `GetLiveContext` shows the exposed entities as a table of name, domain, state, and area. Home Assistant marks `GetLiveContext` read-only, so it works in plan mode.

A call whose `domain` argument names `lock`, `alarm_control_panel`, `cover`, or `valve` always asks, even with an allow rule, in Auto mode, and under autopilot. Only [YOLO mode](/docs/permissions/#yolo-mode) runs it without asking. Calls without a `domain` follow your normal rules.

## Project server trust

Caudra separates server startup trust from tool-call permissions.

Project stdio servers do not start until you review them. Project HTTP servers also wait for plaintext HTTP, URL credentials, failed DNS resolution, or any resolved private or reserved address. A trust decision cannot start or persist until DNS succeeds. Public HTTPS addresses connect normally. Caudra pins the reviewed DNS result for the process, so a later DNS answer cannot redirect that connection into a private network. Global user configuration and ACP-provided runtime servers are trusted for startup.

Open `/mcp` to review a parked server. The picker shows the command or URL, config source, and environment or header names. Values remain hidden.

| Key | Action |
|---|---|
| `o` | Connect once for this process |
| `p` | Confirm trust for this exact configuration and project |
| `r` | Confirm rejection and disable the server |

Persistent trust is stored in the user state directory using the canonical project, server name, and SHA-256 configuration digest. Changing the command, arguments, environment, URL, headers, or OAuth client configuration requires review again. The digest covers values after `{env:NAME}` and `{file:PATH}` references expand, so a global server that needs review, such as a local stdio server in a remote workspace, asks again when a variable or file it reads changes.

MCP and OAuth HTTP requests do not follow redirects. OAuth discovery and token endpoints are resolved and pinned too. A private OAuth endpoint is accepted only on the reviewed MCP origin.

Print, SDK, and ACP sessions fail with an actionable error when project startup trust is still pending. Review the exact configuration in the TUI with `/mcp`, then retry the non-interactive client.

## Runtime toggling

Open the MCP picker with `/mcp`. Turn servers on or off there; changes save back to your config (project or global, depending on which file defined the server).

A running server's row reads like `stdio · 12 tools via mcp as my_server · 3 search-only`. The model reaches its tools through the `mcp` tool, and `as` names the [namespace](#naming-and-namespacing) to call when it differs from the server name. The search-only count is how many tools the main agent's [profile](/docs/system-prompts/#choose-tool-availability) leaves out of the [server list](#the-server-list), as `/tools` reports them.

Select a running server to see what it reported when it connected: its name and version, the protocol version, the features it offers (resources, completions, and list changes), and the first lines of its [instructions](#server-instructions). A [recognised server](#recognised-servers) shows its kind beside its name.

## Status

| Status | Meaning |
|--------|---------|
| Connecting | Waiting for the server to come up |
| Running | Tools available |
| AwaitingTrust | No connection attempted; review with `/mcp` |
| Disabled | Off in config or toggled off in UI |
| Failed | Error shown in UI |
| NeedsAuth | Waiting for OAuth (see below) |
| Refused | Signed in, but the server refused access (see [OAuth](#oauth)) |

If one server fails, the rest still work.

## OAuth

Some HTTP servers need auth. When that happens, Caudra opens your browser to log in. Other servers keep working while you authenticate. Tokens refresh on their own. If you change the server URL, you log in again.

```bash
caudra mcp auth <server-name>     # manually trigger auth
caudra mcp logout <server-name>   # remove stored tokens
```

`caudra mcp auth` checks that the server accepts the stored sign-in, and signs in again when it does not, so you can switch accounts. A server can also turn a valid account away, as Home Assistant's `/api/mcp` does for a user who is not an administrator. When a fresh sign-in is still refused, the command fails, and `/mcp` shows the server as Refused instead of asking you to sign in again.

Before the login, Caudra asks the server where its authorization server is and finds that server's metadata, as the MCP authorization spec describes. Metadata that names a different issuer than the one asked for is rejected, and so is a server that does not support S256 PKCE. Authorization servers may use plain HTTP only on loopback or on the MCP server's own origin, such as a home server at `http://homeassistant.local:8123`.

Caudra then picks the OAuth client in this order:

1. The static client you set as `oauth` for the server.
2. The client this authorization server gave Caudra at the last login.
3. Caudra's client metadata document at `https://caudra.ai/oauth/client-metadata.json`, when the server supports client ID metadata documents. The server fetches that file to check the redirect URI, so it must be able to reach caudra.ai. The file lists the callback port 19876. When that port is busy, Caudra listens on another, which only servers that allow any loopback port accept.
4. A new client from dynamic client registration, when the server offers it.

A server that offers neither, or that cannot reach caudra.ai, needs a client you registered yourself (e.g. your own app on their platform). Add it as `oauth` in the server table, or as a `[mcp.NAME.oauth]` table:

<!-- caudra-docgen:mcp-oauth -->

| Field | Type | Default | Max | Description |
|-------|------|---------|-----|-------------|
| `client_id` | string | required | - | The client ID of the app you registered with the server |
| `client_secret` | string | unset | - | The client secret, for a confidential client. It is stored as plain text unless the global file reads it with `{env:NAME}` from your environment or with `{file:PATH}` from a file |
| `callback_port` | integer | unset | 65535 | Pin the loopback port of the redirect URI, so you can register the URI in advance. Unset tries the default port, then any free port, so the URI can change between runs |
| `callback_path` | string | `/mcp/oauth/callback` | - | The path of the redirect URI. It must start with `/` |
| `callback_hostname` | string | `127.0.0.1` | - | The host name of the redirect URI, such as `localhost` when the server registered that form. The listener still binds to 127.0.0.1 |

<!-- /caudra-docgen:mcp-oauth -->

Set `callback_port` when the server only accepts exact redirect URIs. Otherwise Caudra falls back to its default port, then to any free port, so the redirect URI changes between runs. Set `callback_path` when the server registered a different path (e.g. `/callback`). Set `callback_hostname` to `localhost` when the server registered the name form instead of the IP (the listener still binds to 127.0.0.1).

### Headless machines

On a machine without a browser (say, a dev server over SSH), run `caudra mcp auth <server-name>`. Caudra prints the login URL. Open it on your laptop and log in. The browser lands on a `http://127.0.0.1:19876/...` page that fails to load. Copy that full URL from the address bar and paste it into the terminal to finish the login.

## Resources

A server can offer resources: documents, records, files, and other context to read rather than call. Caudra lists the resources and resource templates of each server that offers them when it connects. A template, such as `repo://{owner}/{name}/issues`, describes a family of URIs.

Mention a resource in a message as `@server:uri`, for example `@docs:guide://install`. Typing `@` lists the resources and templates of running servers beside your project's files, each with its title. Choosing a template types its fixed start. When the server offers completions, its suggestions for the part you are typing join the list. Caudra waits until you pause typing for 150 ms before it asks.

A mention resolves only when its server is running and the URI is one the server listed or one a template describes. Anything else stays plain text, as a path that does not exist does. When a resource mention and a file have the same spelling, the resource wins.

When you send the message, Caudra reads the resource and adds it ahead of your message, the way it adds a [mentioned file](/docs/context/#mention-a-file-with): text inline, and an image as a real image when the model supports vision. Anything else becomes a line with its type and size. Resource text shares the per-turn budget of mentioned files, and text past it is cut with a note.

The model reads resources from Python in [`mcp`](#calling-tools): `await resources(server)` lists the resources and templates a server offers, and `await read_resource(server, uri)` reads one. `servers()` says which servers offer any. Reading works in plan mode and in [read-only agents](/docs/permissions/#read-only-agents). It asks first, and the prompt offers to always allow reads from that server for the conversation, the project, or every project.

## Prompts

MCP servers can expose prompts (reusable message templates). Caudra shows them as slash commands in the command palette: `/server:prompt-name`. Type `/` to filter.

```
/github:create-pr           # no arguments
/analytics:report monthly   # one argument
/review:code src tests      # multiple, positional
```

Skip a required argument and Caudra shows a usage hint. Prompts are fetched when a server starts or reconnects. Editing the server in `mcp.toml` restarts it, and so does turning it off and on in `/mcp`.

When the server offers completions, typing an argument shows the server's suggestions for it, up to 100. Caudra asks once you pause typing for 150 ms, and sends the arguments you already typed with the question. `Up` and `Down` move through the suggestions, `Tab` takes one, and `Enter` runs the command as typed.

A prompt's messages keep their text, their embedded text resources, and their images. An image in a message from you reaches the model as a real image when the model supports vision. Other content becomes a line with its type and size.

## Protocol versions

Caudra speaks MCP `2026-07-28`, and the older protocol that starts with `initialize` (`2025-11-25` and `2025-06-18`), so servers built on either work. When it connects, Caudra asks the server which versions it supports. A server that does not answer that request is spoken to in the older protocol. The choice lasts for the connection, so a server that upgrades gets the newer protocol the next time Caudra connects.

Some parts of MCP are deprecated or have no use in Caudra, and Caudra never tells a server it supports them:

| Feature | What Caudra does |
|---|---|
| Roots | Refuses the request |
| Sampling | Refuses the request |
| Logging | Writes the server's log messages to `caudra.log` at debug level |
| HTTP+SSE transport (`2024-11-05`) | Not supported. Use a server's Streamable HTTP endpoint |
| Dynamic client registration | Used only when an authorization server offers no other way to get a client (see [OAuth](#oauth)) |
