+++
title = "Quick Start"
weight = 1
[extra]
group = "Getting Started"
+++

# Quick Start

Install Caudra, connect a provider, and run a first session. Caudra is an independent fork whose release line starts at `0.1.0`.

## Install

### Linux / macOS

```sh
# Download and read the script first (don't blindly trust shell scripts).
curl -fsSL https://caudra.ai/install.sh -o install.sh
cat install.sh

# Then run.
chmod +x install.sh && sh install.sh
```

One-liner:

```sh
curl -fsSL https://caudra.ai/install.sh | sh
```

Installs to `~/.local/bin`. Override with `CAUDRA_INSTALL_DIR`.

### Windows (PowerShell)

```powershell
# Download and read the script first (don't blindly trust remote scripts).
irm https://caudra.ai/install.ps1 -OutFile install.ps1
Get-Content install.ps1

# Then run.
.\install.ps1
```

One-liner:

```powershell
irm https://caudra.ai/install.ps1 | iex
```

### Windows (Git Bash)

```sh
curl -fsSL https://caudra.ai/install.sh | sh
```

Both install to `%LOCALAPPDATA%\caudra` and add it to your user PATH. Override with `CAUDRA_INSTALL_DIR` / `$env:CAUDRA_INSTALL_DIR`.

### Living on the edge (main branch)

```sh
cargo install --locked --git https://github.com/caudra/caudra.git caudra
```

### With Nix

```sh
nix run github:caudra/caudra
```

Or download a pre-built binary from [GitHub Releases](https://github.com/caudra/caudra/releases/latest).

## Connect a provider

```bash
caudra auth login              # interactive picker (OAuth or API key)
caudra auth login anthropic    # Claude subscription OAuth
export ANTHROPIC_API_KEY=... # or just export a key
```

Anthropic, OpenAI, Google, Ollama, and friends all work; multiple keys in one var rotate on rate limits. Every env var and model catalog is in [Providers](/docs/providers/).

## First session

From a repo:

```bash
caudra
```

Type what you want done, press Enter, watch it work. Worth knowing on day one:

- **Permissions.** File edits inside the repo run freely. `shell` and web tools ask first: `y` allows once, `s` remembers the exact call for the conversation, and `a` remembers it for the project. Deny rules always win. `/yolo` skips prompts after deny checks. Details in [Permissions](/docs/permissions/).
- **Plan mode.** `Tab` toggles it. The agent may only write the plan file until you approve, then back to build mode.
- **Models.** `/model` switches mid-session.
- **Sessions.** `/new` starts a second session while the first keeps working in the background; `/sessions` jumps between them. Tomorrow, `caudra --continue` resumes where you left off.
- **Message actions.** Right-click a conversation message, or hold the left mouse button for half a second, to fork or revert there. See [Sessions, Forks, and Revert](/docs/sessions/) for history boundaries, file snapshots, conflicts, and unrevert.
- **Queue and steering.** While Caudra works, `Enter` sends the prompt next, `Alt+S` guides the current run, and `Alt+X` stops and replaces it. See [Queue and Steering](/docs/queue/).
- **Tasks.** Click a task call to inspect its subagent transcript. Send guidance from the task input while it runs, then click `[< Main]` to return. `/tasks` or `Ctrl+X` opens every task. Details in [Commands](/docs/commands/#tasks).
- **Your shell.** Prefix input with `!` to run a command yourself (`!cargo test`). `!!` hides command and output from the agent.
- **Escape hatch.** `Esc Esc` cancels a streaming response. When idle, it rewinds instead.
- **Help.** `Ctrl+H` lists every keybinding, or see [Keybindings](/docs/keybindings/).

## Default model (optional)

```lua
-- ~/.config/caudra/init.lua
caudra.setup({
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
})
```

Without it, Caudra remembers the last model you used.

## Teach it your project

Caudra loads `AGENTS.md` (or `CLAUDE.md`, `.cursorrules`, and friends) from your repo automatically. Per-project settings live under `.caudra/`:

```
.caudra/
├── init.lua           # overrides global config
├── permissions.toml   # restrictive project permission policy
├── mcp.toml           # MCP server config
├── commands/          # custom slash commands (.md files)
└── skills/            # project skills (each dir has a SKILL.md)
AGENTS.md              # always in context
AGENTS.local.md        # personal per-project instructions (gitignored)
```

Which instruction file wins, when subdirectory rules load, and how skills and memory fit together: [Context](/docs/context/). All settings: [Configuration](/docs/configuration/).
