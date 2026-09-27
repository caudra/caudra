+++
title = "ACP"
weight = 22
[extra]
group = "Guides"
+++

# ACP (Agent Client Protocol)

Run Caudra inside your editor. `caudra acp` starts an [ACP](https://agentclientprotocol.com/) server over stdio, so any ACP-capable editor (like [Zed](https://zed.dev/)) can drive Caudra as its coding agent.

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

## What works

- **Sessions persist.** Loading a session replays the full conversation in the editor, so you can resume where you left off.
- **Model switching.** Pick a model from the editor's dropdown, mid-session. All configured providers show up. Providers that list their models over the wire (OpenRouter and friends) are discovered in the background, so the dropdown keeps filling up for a moment after the session starts, one provider at a time.
- **Modes.** Switch between build (full access) and plan (plan-file writes only) from the editor.
- **Permissions.** Tool permission prompts appear in the editor. "Allow exact call for conversation" stores only the reviewed input and resources. Broad tool authority is not exposed through ACP.
- **Questions.** The `question` tool becomes a native form in the editor (ACP elicitation). If the client does not support elicitation, the tool is dropped and the model asks in plain text.
- **Live tool calls.** Tool progress streams as it happens, including sub-agents and batched calls.
- **Images and context.** Prompts can include images and editor-attached files.

Authentication, providers, and permissions come from your normal Caudra config. Set up [providers](/docs/providers/) first and ACP sessions just work.

Project MCP startup trust must already be approved through `/mcp` in the TUI. ACP returns an actionable session error instead of silently omitting a parked server.

ACP supports synchronous subagents and shell commands, but not [background work](/docs/sessions/#background-tasks) or workflows. The `auto` execution policies resolve to synchronous execution here. Strict `async` withholds the affected tool and rejects stale calls rather than changing its execution policy. Choose `sync` or `auto`, or use the TUI or [stream-JSON SDK](/docs/headless/#background-tasks) for session-owned work that continues after a parent answer.

```bash
caudra acp
caudra acp -m anthropic/claude-sonnet-4-6
caudra acp --yolo
caudra --no-jit acp
```

`caudra acp` only takes `-m` / `--model` and `--yolo` as subcommand flags. Global flags like `--no-jit` must come before the subcommand (`caudra --no-jit acp`, not `caudra acp --no-jit`).

Plan mode in ACP uses the same state-directory plan files as the TUI (`…/projects/<project-id>/plans/<slug>.md`) rather than the SDK's `./plan.md`.
