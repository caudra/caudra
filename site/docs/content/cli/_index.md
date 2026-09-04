+++
title = "CLI"
weight = 11
[extra]
group = "Reference"
+++

# CLI

`caudra` without a subcommand starts the TUI. Subcommands cover auth, models, MCP OAuth, updates, and a few debug helpers. Many flags only apply to one of three run paths: **TUI**, one-shot **`--print`**, or **SDK** (`--print --input-format stream-json`).

```bash
caudra [OPTIONS] [PROMPT]
caudra <COMMAND>
```

If you pass a prompt (or pipe stdin) without `--print`, the TUI still opens and that text is the first message. With `--print`, Caudra runs non-interactively and exits when done.

When interactive Caudra starts in a Herdr pane, it automatically reports native `caudra` lifecycle state through Herdr's inherited environment and public custom-agent API. `--print` and SDK mode do not claim pane lifecycle authority.

## Flags by run path

| Flag | TUI | `--print` | SDK (`stream-json`) |
|------|-----|-----------|---------------------|
| `-m` / `--model` | yes | yes | yes |
| `--yolo` | yes | yes | yes (or `--permission-mode bypassPermissions`) |
| `--no-plugins` / `--no-commands` / `--no-jit` | yes | yes | yes |
| `--allowed-tools` / `--disallowed-tools` | yes | yes | yes |
| `--system-prompt-profile` | yes | yes | yes |
| `--ephemeral` | yes | yes | yes |
| `-c` / `--continue`, `-s` / `--session` | yes | no (always new session) | yes |
| `--exit-on-done` | yes | n/a (always exits) | n/a |
| `--image` | no (use Ctrl+V paste) | yes | via wire protocol |
| `--verbose`, `--output-format` | no | yes | stream only |
| `--system-prompt`, `--append-system-prompt` | no | no | yes |
| `--max-turns`, `--session-id`, `--fork-session` | no | no | yes |
| `--permission-mode` | no | no | yes |
| `--include-partial-messages` | no | no | yes |

### Shared flags (detail)

| Flag | Description |
|------|-------------|
| `-p`, `--print` | Non-interactive run. See [Headless Mode](/docs/headless/) |
| `--ephemeral` | Store the session, outputs, snapshots, input history, and stash in a temporary root removed at exit. Credentials, trust, and preferences remain persistent |
| `--image <PATH>` | Attach an image in `--print` mode (repeatable). Paths must be png, jpeg, gif, or webp |
| `-m`, `--model <SPEC>` | Model as `provider/model-id`. Fallback: last used → `provider.default_model` in config → auto-detect from available providers |
| `--verbose` | Full turn-by-turn messages in `--print` output |
| `-c`, `--continue` | Resume the most recent session in this directory (TUI / SDK only) |
| `-s`, `--session` / `--resume <ID>` | Resume a specific session (TUI / SDK only) |
| `--output-format <text\|json\|stream-json>` | Output shape for `--print` (default `text`) |
| `--input-format <text\|stream-json>` | With `--print`, `stream-json` enters SDK mode |
| `--no-commands` | Skip custom commands from `.caudra/commands`, `.claude/commands`, etc. |
| `--no-plugins` | Skip user `init.lua` (global and project). The Lua host stays up and every built-in tool is native, so nothing else is lost |
| `--no-jit` | Run plugin Lua on the interpreter with full debug info |
| `--yolo` | Skip permission prompts on gated tools (alias: `--dangerously-skip-permissions`). Deny rules still apply |
| `--exit-on-done` | Exit when the agent finishes (TUI automation wrappers) |
| `--allowed-tools <LIST>` | Comma-separated allow list (PascalCase or snake_case) |
| `--disallowed-tools <LIST>` | Comma-separated deny list |
| `--system-prompt-profile <NAME>` | Select a profile from the user `system-prompts` config directory. See [System Prompt Profiles](/docs/system-prompts/) |
| `--session-id <ID>` | Session id for SDK mode |
| `--fork-session` | Load a session's history under a new id (SDK) |
| `--max-turns <N>` | Cap agent turns (SDK) |
| `--system-prompt <TEXT>` | Replace the system prompt (SDK only) |
| `--append-system-prompt <TEXT>` | Append to the built-in system prompt (SDK only) |
| `--permission-mode <MODE>` | SDK: `default`, `acceptEdits`, `plan`, or `bypassPermissions` |
| `--include-partial-messages` | Stream partial deltas in SDK mode |

### Tool name lists

`--allowed-tools` / `--disallowed-tools` accept PascalCase (`FileRead,FileEdit,Shell`) or snake_case (`file_read,file_edit,shell`). Caudra converts PascalCase to snake_case and checks the result against the built-in tool names. Unknown names fail with the complete list of valid names.

### Permission modes (SDK)

| Mode | Effect |
|------|--------|
| `default` | Normal permission prompts |
| `acceptEdits` | Accepted for Claude Code compatibility; currently same as `default` |
| `plan` | Agent mode plan with plan file `./plan.md` under cwd |
| `bypassPermissions` | Same as `--yolo` for the SDK path |

If both `--yolo` and `--permission-mode` are set, the explicit mode wins. Unknown mode names warn and fall back to `default`.

Several other Claude Code flags are accepted and ignored so existing scripts keep parsing. Caudra prints a warning when you pass one of them.

## Subcommands

### `caudra auth`

```bash
caudra auth login [provider]   # interactive picker if omitted
caudra auth logout <provider>
caudra auth status
```

`login` stores credentials under the state directory and can write plan / base URL choices into `providers.toml` (see [Configuration](/docs/configuration/#directory-layout) for the platform path). Anthropic, OpenAI, xAI, and Copilot have dedicated flows when named explicitly. Other providers prompt for a key and a plan when the provider has more than one. Custom providers can be created from the interactive picker.

`caudra auth login anthropic` starts experimental Claude subscription OAuth. The command explains the Anthropic terms limitation before opening the browser. Use the interactive picker or `ANTHROPIC_API_KEY` for API-key auth.

`status` shows each provider as configured (key on disk), env-only, or missing.

### `caudra models`

Lists every model Caudra currently knows about (built-ins, discovered, catalog). One model spec per line. Warnings from discovery go to stderr.

### `caudra mcp`

```bash
caudra mcp auth <server>     # OAuth for an HTTP MCP server
caudra mcp logout <server>   # drop stored tokens
```

Server names come from your [MCP config](/docs/mcp/). On a machine without a browser, `auth` prints a URL you open elsewhere and paste back.

### `caudra update` / `caudra rollback`

```bash
caudra update            # install latest release
caudra update -y         # skip confirmation
caudra update --no-color
caudra rollback          # previous version
```

Uses the same install locations as the install scripts.

### `caudra acp`

```bash
caudra acp
caudra acp -m anthropic/claude-sonnet-4-6
caudra acp --yolo
caudra --ephemeral acp
caudra --no-jit acp
```

Starts an [ACP](/docs/acp/) server on stdio for editors like Zed. Subcommand flags are only `-m` / `--model` and `--yolo`. Global flags such as `--ephemeral` and `--no-jit` must come before the subcommand.

### `caudra index`

```bash
caudra index path/to/file.rs
```

Runs the native `index` tool and prints its compact file skeleton or directory listing. It honors `plugins.index.enabled` and `plugins.index.max_file_size_mb`. `--no-plugins` skips user `init.lua`, so default index settings apply.

### `caudra prompt`

```bash
caudra prompt                  # rendered system prompt (default: system variant)
caudra prompt research
caudra prompt general
caudra prompt --plan           # system prompt + plan-mode reminder (system only)
caudra prompt --tools          # tool definitions as JSON
caudra prompt --tools --names  # tool names only, one per line
```

Debug helper for inspecting the prompt and tool surface the agent sees. The `research` and `general` variants include the selected system prompt profile and their final host mode contract. `--plan` is rejected on non-system variants.

### `caudra storage`

```bash
caudra storage path                       # session database path
caudra storage stats [--json]             # rows, bytes, artifacts, pending cleanup
caudra storage check                      # integrity check
caudra storage sessions [--directory DIR] # list sessions with activity, size, state
caudra storage trim   [POLICY] [--dry-run]
caudra storage forget [POLICY | ID...] [--dry-run] [--prune]
caudra storage prune  [--dry-run]
caudra storage pin <ID>...
caudra storage unpin <ID>...
caudra storage checkpoint [--truncate]
caudra storage vacuum [--pages N]
caudra storage usage  [--group-by GROUP] [--since DURATION] [--json]
caudra storage usage  --prune-older-than DURATION
```

`trim` demotes sessions to the transcript tier and `forget` deletes them. Both take a keep policy in `restic forget` terms and fall back to the configured `storage.retention` policy when no `--keep-*` flag is given. `prune` reclaims space that no session references. See [Sessions](/docs/sessions/#retention) for the policy rules and what each tier keeps.

| Flag | Description |
|------|-------------|
| `--keep-last <N>` | Keep the N most recently active sessions |
| `--keep-hourly`, `--keep-daily`, `--keep-weekly`, `--keep-monthly`, `--keep-yearly <N>` | For the last N periods that contain sessions, keep the newest session of each |
| `--keep-within <DURATION>` | Keep every session active within the duration, for example `90d` or `2y5m7d3h` |
| `--keep-within-hourly` ... `--keep-within-yearly <DURATION>` | Keep one session per period within the duration |
| `--group-by <directory\|none>` | Evaluate the policy per working directory (default) or across every session |
| `--directory <DIR>` | Only sessions for one working directory |
| `--dry-run` | Print the plan and change nothing |
| `--json` | Emit the plan and outcomes as JSON |
| `--prune` | `forget` only: run `prune` when at least one session was forgotten |
| `--unsafe-allow-remove-all` | Allow an empty policy, which keeps nothing. Requires `--directory` |

A session is kept when any rule matches. Pinned sessions, sessions open in any Caudra process, and sessions with a pending revert are never trimmed or forgotten by policy. `forget <ID>` refuses pinned sessions.

`usage` reports spend from a ledger that outlives the sessions that produced it, so trimming and forgetting leave the numbers intact. Group by `model` (default), `provider`, `project`, `day`, `month`, or `total`, narrow with `--since 30d`, and trim the ledger itself with `--prune-older-than`, which takes no other flag. [Token Economy](/docs/token-economy/#lifetime-spend) explains what the columns mean.

## Everyday examples

```bash
# TUI on a project
cd ~/code/my-app && caudra

# One-shot with YOLO and a model pin
caudra -p --yolo -m anthropic/claude-sonnet-4-6 "summarize the architecture"

# Resume yesterday's session
caudra --continue

# List models, then log in
caudra models
caudra auth login

# Inspect tools without starting a session
caudra prompt --tools --names
```

For JSON / stream-json output, stdin prompts, and SDK wire mode, see [Headless Mode](/docs/headless/).
