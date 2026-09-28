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
caudra auth login                           # choose a provider and auth method
caudra auth login anthropic                 # Claude subscription OAuth
caudra auth login openai                    # ChatGPT/Codex subscription OAuth
caudra auth login anthropic --method api-key
```

Open `/login` inside the TUI for the same OAuth and API-key choices. You can also set provider environment variables such as `ANTHROPIC_API_KEY`. Multiple keys in one variable rotate on rate limits. Every env var and model catalog is in [Providers](/docs/providers/).

## First session

From a repo:

```bash
caudra
```

Type what you want done, press Enter, watch it work. Worth knowing on day one:

- **Permissions.** File edits inside the repo run freely. `shell` and web tools ask first: `y` allows once, `s` remembers the exact call for the conversation, and `a` remembers it for the project. Deny rules always win. `/yolo` skips prompts after deny checks. Details in [Permissions](/docs/permissions/).
- **Plan mode.** Caudra opens here, so the plan file is the only thing the agent writes until you approve. `Ctrl+O` opens the plan in the [workbench](/docs/workbench/#plans-memory-notes-and-prompt-drafts) to read or edit before you do. The agent reads and searches freely, and runs read-only shell commands like `git log` and `rg`. Anything else asks first. You can approve it for the conversation, though not for the project or globally, so nothing allowed while planning outlives the session. `Tab` toggles it, taking effect with your next message: the status bar reads `[PLAN→BUILD]` while the switch is still pending. The toggle also brings back the model that mode was last used with, shown the same way as `[claude-opus-5→claude-sonnet-5]`. A resumed session reopens in the mode you left it in.
- **Models.** `/model` switches the Chat model and assigns models to jobs such as Plan and Subagent. See [Providers](/docs/providers/#model-jobs).
- **Status bar.** The footer has two rows. The top row shows what your next message runs with: the mode, the model, and the reasoning level, plus `[fast]` and `[yolo]` while they are on. The bottom row shows what is happening now: running tasks, shells, and workflows, any retry or error, and the context and cost meters. When the terminal narrows, each row shortens only its own chips, so a long error never hides the model. A terminal shorter than 20 rows keeps a single row. Clicking a chip changes that setting or opens its view.
- **Sessions.** `/new` starts a second session while the first keeps working in the background; `/sessions` jumps between them. Tomorrow, `caudra --continue` resumes where you left off.
- **Message actions.** Click `⋮` beside a conversation message to fork or revert there. Right-clicking the message is an optional shortcut when the terminal forwards it. See [Sessions, Forks, and Revert](/docs/sessions/) for history boundaries, file snapshots, conflicts, and unrevert.
- **Queue and steering.** While Caudra works, `Enter` sends the prompt next, `Ctrl+X g` guides the current run, and `Ctrl+X x` stops and replaces it. See [Queue and Steering](/docs/queue/).
- **Tasks.** Click a task call to inspect its subagent transcript. Send guidance from the task input while it runs, then click `[< Main]` to return. `/tasks`, `Ctrl+X a`, or the task count above the input opens every task. See [task navigation](/docs/commands/#tasks) and [background tasks](/docs/sessions/#background-tasks).
- **Dismissing a modal.** `Esc` closes the panel or picker in front of you, and so does a press anywhere outside it. That press is swallowed, so it does not also act on what is under it. The permission prompt, the question form, the plan form, and the paste editor stay where they are, because each is waiting on an answer or holding text only you have. On the question form, `Esc` answers nothing and the agent carries on. `Ctrl+C` stops the agent that asked, so the run waits for your next message.
- **Your shell.** Prefix input with `!` to run a command yourself (`!cargo test`). `!!` hides command and output from the agent.
- **Escape hatch.** `Esc Esc` stops the main run and all session tasks and workflows. With no session work left, it opens rewind instead. See [Stop and replace](/docs/queue/#stop-and-replace).
- **Help.** `F1` lists every keybinding, or see [Keybindings](/docs/keybindings/).

## Default model (optional)

```lua
-- ~/.config/caudra/init.lua
caudra.setup({
    provider = {
        default_model = "anthropic/claude-sonnet-4-6",
    },
})
```

Without it, Caudra remembers the last model you used in each mode. Pick a strong model while planning and a cheaper one for building, and `Tab` moves between them with you.

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
