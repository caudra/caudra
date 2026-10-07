---
title: "Quick Start"
description: "Install, connect a provider, first session."
---

Install Caudra, connect a provider, and run a first session. Caudra is an independent fork. Its first public release is 0.2 Preview (`0.2.0-preview.1`).

## Install

The installers prefer a stable release. While only previews exist, they select the newest preview and label it as such. Network failures stop installation rather than changing channels. Downloaded archives are checked against the release SHA-256 manifest before extraction.

To select a channel explicitly after downloading the shell installer, run `sh install.sh --channel preview` or `sh install.sh --channel stable`. To install an exact version, run `sh install.sh v0.2.0-preview.1`. PowerShell accepts the same arguments, for example `./install.ps1 --channel preview`.

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

This build leaves out [`python_execution`](/docs/tools/#python_execution), because only release builds, Nix, and `make install` from a checkout embed its worker. The other tools work as usual.

### With Nix

```sh
nix run github:caudra/caudra
```

Or download a pre-built binary from [GitHub Releases](https://github.com/caudra/caudra/releases).

The interactive UI checks for updates in the background by default. It never installs an update automatically. Set `ui.update_check = false` in your config or `CAUDRA_ENABLE_UPDATE_CHECK=0` for one run to disable the check. See [update settings](/docs/configuration/#ui-update-check) and [manual updates](/docs/cli/#caudra-update-caudra-rollback).

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

- **Permissions.** File edits inside the repo run freely. `shell` and web tools ask first: `y` allows once, `s` remembers the scope shown between `‹` and `›` for the conversation, and `a` remembers it for the project. `←` broadens that scope and `→` narrows it. Deny rules always win. `/yolo` skips prompts after deny checks. Details in [Permissions](/docs/permissions/).
- **Plan mode.** Caudra opens here, so the plan file is the only thing the agent writes until you approve. `Ctrl+O` opens the plan in the [workbench](/docs/workbench/#plans-memory-notes-and-prompt-drafts) to read or edit before you do. The agent reads and searches freely, and runs read-only shell commands like `git log` and `rg`. Anything else asks first. You can approve it for the conversation, or keep a narrow scope such as `git log *` for the project. A broad scope lasts the conversation at most, and nothing allowed while planning applies to all projects. `/yolo` runs such commands without asking and stores no rule. `Tab` toggles plan mode, taking effect with your next message: the status bar reads `[PLAN→BUILD]` while the switch is still pending. The toggle also brings back the model that mode was last used with, shown the same way as `[claude-opus-5→claude-sonnet-5]`. A resumed session reopens in the mode you left it in.
- **Models.** `/model` switches the Chat model and assigns models to jobs such as Plan and Subagent. See [Providers](/docs/providers/#model-jobs).
- **Status bar.** The footer has two rows. The top row shows what your next message runs with: the mode, the model, and the reasoning level, plus `[fast]` and `[yolo]` while they are on. The bottom row shows what is happening now: running tasks, shells, and workflows, any retry or error, and the context and cost meters. When the terminal narrows, each row shortens only its own chips, so a long error never hides the model. A terminal shorter than 20 rows keeps a single row. Clicking a chip changes that setting or opens its view.
- **Sessions.** `/new` starts a second session while the first keeps working in the background; `/sessions` jumps between them. Tomorrow, `caudra --continue` resumes where you left off.
- **Message actions.** Click `⋮` beside a conversation message to fork or revert there. Right-clicking the message is an optional shortcut when the terminal forwards it. See [Sessions, Forks, and Revert](/docs/sessions/) for history boundaries, file revert, conflicts, and unrevert.
- **Queue and steering.** While Caudra works, `Enter` sends the prompt next, `Ctrl+X g` guides the current run, and `Ctrl+X x` stops and replaces it. See [Queue and Steering](/docs/queue/).
- **Tasks.** Click a task call to inspect its subagent transcript. Send guidance from the task input while it runs, then click `[< Main]` to return. `/tasks`, `Ctrl+X a`, or the task count above the input opens every task. `/shells` or the shell count lists the shell commands the agents ran, where you can stop a running one. See [task navigation](/docs/commands/#tasks), [shell commands](/docs/commands/#shell-commands), and [background tasks](/docs/sessions/#background-tasks).
- **Dismissing a modal.** `Esc` closes the panel or picker in front of you, and so does a press anywhere outside it. That press is swallowed, so it does not also act on what is under it. The permission prompt, the question form, the plan form, and the paste editor stay where they are, because each is waiting on an answer or holding text only you have. On the question form, `Esc` answers nothing and the agent carries on. `Ctrl+C` stops the agent that asked, so the run waits for your next message.
- **Your shell.** Prefix input with `!` to run a command yourself (`!cargo test`). `!!` hides command and output from the agent.
- **Escape hatch.** `Esc Esc` stops the main run and all session tasks and workflows. With no session work left, it opens rewind instead. See [Stop and replace](/docs/queue/#stop-and-replace).
- **Help.** `F1` lists every keybinding, or see [Keybindings](/docs/keybindings/).
- **These docs.** `/docs` opens this manual inside Caudra, for the version you run, and `/` searches every page. The agent reads the same pages through the [`caudra-docs` skill](/docs/skills/#caudra-docs).

## Default model (optional)

```toml
# ~/.config/caudra/caudra.toml
[provider]
default_model = "anthropic/claude-sonnet-4-6"
```

Without it, Caudra remembers the last model you used in each mode. Pick a strong model while planning and a cheaper one for building, and `Tab` moves between them with you.

Settings live in `caudra.toml` (Windows: `%APPDATA%\caudra\caudra.toml`) and need no Lua. Workflows, managed sandboxes, direct remote Workcell connections, Lua plugins, the decision engine, and cross-session messages are experimental and stay off until you turn them on in the global file. See [Experimental features](/docs/configuration/#experimental-features).

## Teach it your project

Caudra loads `AGENTS.md` (or `CLAUDE.md`, `.cursorrules`, and friends) from your repo automatically. Per-project settings live under `.caudra/`:

```
.caudra/
├── caudra.toml        # overrides global settings
├── permissions.toml   # restrictive project permission policy
├── mcp.toml           # MCP server config
├── commands/          # custom slash commands (.md files)
└── skills/            # project skills (each dir has a SKILL.md)
AGENTS.md              # always in context
AGENTS.local.md        # personal per-project instructions (gitignored)
```

Which instruction file wins, when subdirectory rules load, and how skills and memory fit together: [Context](/docs/context/). All settings: [Configuration](/docs/configuration/).
