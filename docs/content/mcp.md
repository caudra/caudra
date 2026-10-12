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

The TUI applies an edit to either file while it runs. It starts new servers, including the first one when Caudra started with none. It stops removed or disabled ones, and restarts a server whose definition changed. Servers you did not touch keep their connection. A file that fails to load keeps the servers it last loaded, and the error shows in `/mcp`. A changed project server that needs review waits in `/mcp`, and a `[review MCP]` chip in the status bar opens it. The model sees the new tools after its next rebuild, see [Add an MCP server](/docs/configuration/#add-an-mcp-server).

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

In the global `mcp.toml`, `{env:NAME}` takes a value from your environment or the global `.env`, so the token stays out of the file. A project `mcp.toml` cannot read the environment, and Caudra loads none of its servers while it holds a reference. See [Environment variables](/docs/configuration/#environment-variables).

Some HTTP servers need OAuth but have no dynamic client registration. For those, give Caudra a static client:

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
| `always_load` | bool | `false` | - | - | Load every tool of the server up front instead of through `tool_search` |
| `command` | string[] | required | - | - | Stdio servers: the program and its arguments. It must not be empty. When `url` is also set, `command` wins |
| `environment` | table | `{}` | - | - | Stdio servers: environment variables for the server process, such as `{ GITHUB_TOKEN = "{env:GITHUB_TOKEN}" }`. In the global file, `{env:NAME}` reads a value from your environment. Other values are stored as plain text |
| `url` | string | required | - | - | HTTP servers: the server URL. It must start with `http://` or `https://` |
| `headers` | table | `{}` | - | - | HTTP servers: headers sent with every request, such as `{ Authorization = "Bearer {env:GITHUB_TOKEN}" }`. In the global file, `{env:NAME}` reads a value from your environment. Other values are stored as plain text |
| `oauth` | table | unset | - | - | HTTP servers: a static OAuth client, for a server that has no dynamic client registration |

<!-- /caudra-docgen:mcp-server-fields -->

Set `command` for stdio, `url` for HTTP. Pick one.

Keys at the top level of `mcp.toml`, outside any server:

<!-- caudra-docgen:mcp-top-level -->

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `defer_tools` | integer | `10` | Defer MCP tools behind `tool_search` only when the servers offer more than this many. `0` always defers. A project value replaces the global one |

`caudra config example mcp` prints every `mcp.toml` key with its default, all commented out. [Reference configs](/docs/reference-configs/#mcp-toml) shows the same text.

<!-- /caudra-docgen:mcp-top-level -->

The file can start with `version = 1`, and a file without it counts as version 1. When the version is newer than this build reads, no server in the file starts and Caudra shows the error. See [Config file versions](/docs/configuration/#config-file-versions).

## Tool search

Every tool definition a server exposes costs context window space, on every request. Take Datadog's MCP server: with all toolsets on it ships over 100 tools, when a task often needs three.

So Caudra, like Claude Code, defers MCP tools by default. The model sees one small `tool_search` tool that lists the deferred names, searches when it actually needs something, and the matches stay loaded for the rest of the session. Resume a session and the tools it was using come back. Subagents keep their own loads, so their searches don't bloat your main conversation.

```
server ships 117 tool definitions
        │
  more than defer_tools (10)?
   │ no          │ yes
   ▼             ▼
   all load      context gets one small tool: tool_search
   upfront       │
                 │  model: tool_search("logs")
                 ▼
                 3 matches load, stay for the session
                 114 definitions never enter context
```

You don't configure anything for this. Add the server as usual:

```toml
[mcp.datadog]
url = "https://mcp.datadoghq.com/api/unstable/mcp-server/mcp?toolsets=all"
```

Ask about an incident, and the model searches for something like `datadog logs`, gets back the few matching tools, and the other hundred definitions never enter the conversation.

With 10 or fewer tools across all your servers there is no search step: at that size, searching costs more than it saves, so everything loads upfront. The top-level `defer_tools` key moves that line:

```toml
defer_tools = 30

[mcp.github]
url = "https://api.githubcopilot.com/mcp/"
```

Set it to 0 to always defer, or above your tool count to never defer.

If one server should skip the search step entirely, opt it out:

```toml
[mcp.linear]
command = ["linear-mcp-server"]
always_load = true
```

Good for small servers you rely on every turn. On a big server it defeats the point: every definition is back in your context on every request.

## Naming and namespacing

Server names are ASCII alphanumeric, hyphens ok (no dots). Tools get prefixed with their server name: a `read` tool on the `filesystem` server becomes `filesystem__read`. Because of this, `__` is reserved and names can't collide with built-in tools.

See [Permissions](/docs/permissions/#mcp-tool-calls) for MCP call review and remembered decisions.

Tool-call approval binds to the configured server authority, the discovered tool contract, and one transport snapshot. Reconnects and contract changes cannot inherit an approval by reusing the same display name.

Generic MCP approvals are exact-input by default. The permission review shows the complete validated JSON rather than a truncated preview.

## Results in the transcript

An MCP call's card is labelled `server › tool`, with short arguments beside the label. Opening the card shows every argument. A multi-line string argument, such as a script, is shown as plain text under its name.

Each block of the result is drawn by its type. A text block that is a whole JSON object or array is indented and coloured. Other text is rendered as markdown. Images, audio, and resources become labelled rows with their type and size. The model receives the same text whatever the card shows, and it never receives image or audio data.

## Project server trust

Caudra separates server startup trust from tool-call permissions.

Project stdio servers do not start until you review them. Project HTTP servers also wait for plaintext HTTP, URL credentials, failed DNS resolution, or any resolved private or reserved address. A trust decision cannot start or persist until DNS succeeds. Public HTTPS addresses connect normally. Caudra pins the reviewed DNS result for the process, so a later DNS answer cannot redirect that connection into a private network. Global user configuration and ACP-provided runtime servers are trusted for startup.

Open `/mcp` to review a parked server. The picker shows the command or URL, config source, and environment or header names. Values remain hidden.

| Key | Action |
|---|---|
| `o` | Connect once for this process |
| `p` | Confirm trust for this exact configuration and project |
| `r` | Confirm rejection and disable the server |

Persistent trust is stored in the user state directory using the canonical project, server name, and SHA-256 configuration digest. Changing the command, arguments, environment, URL, headers, or OAuth client configuration requires review again. The digest covers values after `{env:NAME}` expands, so a global server that needs review, such as a local stdio server in a remote workspace, asks again when a variable it reads changes.

MCP and OAuth HTTP requests do not follow redirects. OAuth discovery and token endpoints are resolved and pinned too. A private OAuth endpoint is accepted only on the reviewed MCP origin.

Print, SDK, and ACP sessions fail with an actionable error when project startup trust is still pending. Review the exact configuration in the TUI with `/mcp`, then retry the non-interactive client.

## Runtime toggling

Open the MCP picker with `/mcp`. Turn servers on or off there; changes save back to your config (project or global, depending on which file defined the server).

## Status

| Status | Meaning |
|--------|---------|
| Connecting | Waiting for the server to come up |
| Running | Tools available |
| AwaitingTrust | No connection attempted; review with `/mcp` |
| Disabled | Off in config or toggled off in UI |
| Failed | Error shown in UI |
| NeedsAuth | Waiting for OAuth (see below) |

If one server fails, the rest still work.

## OAuth

Some HTTP servers need auth. When that happens, Caudra opens your browser to log in. Other servers keep working while you authenticate. Tokens refresh on their own. If you change the server URL, you log in again.

```bash
caudra mcp auth <server-name>     # manually trigger auth
caudra mcp logout <server-name>   # remove stored tokens
```

Servers without dynamic client registration need a client you registered yourself (e.g. your own app on their platform). Add it as `oauth` in the server table, or as a `[mcp.NAME.oauth]` table, so the auth flow uses it instead of trying to register:

<!-- caudra-docgen:mcp-oauth -->

| Field | Type | Default | Max | Description |
|-------|------|---------|-----|-------------|
| `client_id` | string | required | - | The client ID of the app you registered with the server |
| `client_secret` | string | unset | - | The client secret, for a confidential client. It is stored as plain text unless the global file reads it from your environment with `{env:NAME}` |
| `callback_port` | integer | unset | 65535 | Pin the loopback port of the redirect URI, so you can register the URI in advance. Unset tries the default port, then any free port, so the URI can change between runs |
| `callback_path` | string | `/mcp/oauth/callback` | - | The path of the redirect URI. It must start with `/` |
| `callback_hostname` | string | `127.0.0.1` | - | The host name of the redirect URI, such as `localhost` when the server registered that form. The listener still binds to 127.0.0.1 |

<!-- /caudra-docgen:mcp-oauth -->

Set `callback_port` when the server only accepts exact redirect URIs. Otherwise Caudra falls back to its default port, then to any free port, so the redirect URI changes between runs. Set `callback_path` when the server registered a different path (e.g. `/callback`). Set `callback_hostname` to `localhost` when the server registered the name form instead of the IP (the listener still binds to 127.0.0.1).

### Headless machines

On a machine without a browser (say, a dev server over SSH), run `caudra mcp auth <server-name>`. Caudra prints the login URL. Open it on your laptop and log in. The browser lands on a `http://127.0.0.1:19876/...` page that fails to load. Copy that full URL from the address bar and paste it into the terminal to finish the login.

## Prompts

MCP servers can expose prompts (reusable message templates). Caudra shows them as slash commands in the command palette: `/server:prompt-name`. Type `/` to filter.

```
/github:create-pr           # no arguments
/analytics:report monthly   # one argument
/review:code src tests      # multiple, positional
```

Skip a required argument and Caudra shows a usage hint. Prompts are fetched when a server starts or reconnects. Editing the server in `mcp.toml` restarts it, and so does turning it off and on in `/mcp`. Only text content is supported.
