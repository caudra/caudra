+++
title = "Headless Mode"
weight = 21
[extra]
group = "Guides"
+++

# Headless Mode

Run Caudra non-interactively with `--print` / `-p`. Useful for scripts, CI, and automation.

```bash
caudra --print --prompt "explain this codebase"
```

Pipe via stdin:

```bash
echo "list all TODO comments" | caudra -p
```

With both, the piped text is appended after `--prompt`, which lets you attach command output to an instruction. Running `--print` with neither is an error.

## Output Formats

| Format | Description |
|--------|-------------|
| `text` | Raw response only (default) |
| `json` | Single JSON object with metadata |
| `stream-json` | JSONL stream, one event per line |

```bash
caudra --print --output-format json --prompt "fix the tests"
```

JSON output includes `type`, `subtype`, `is_error`, `duration_ms`, `num_turns`, `result`, `stop_reason`, `session_id`, `total_cost_usd`, `subscription_cost_usd`, and `usage`.

`total_cost_usd` is money owed. When a subscription covers the run, its list
price lands in `subscription_cost_usd` instead, and the two are never added
together. See [Token economy](/docs/token-economy/#spend-on-a-subscription).

Add `--verbose` to include full turn-by-turn messages in the output.

## Claude Code Compatibility

Caudra's `--print` is a drop-in replacement for Claude Code:

```bash
# Before
claude "fix the bug" --print --output-format json

# After
caudra --print --output-format json --prompt "fix the bug"
```

Same JSON fields, same `--output-format` options, same `--verbose` behavior. Scripts that parse Claude Code output work unchanged. The prompt itself moves to `--prompt`, because Caudra reads a bare word as a subcommand.

## SDK / Stream Mode

For tools like Conductor, Windsurf, or custom orchestrators that speak the Claude Code SDK wire protocol, use `--input-format stream-json`:

```bash
caudra --print --input-format stream-json
```

This enters a bidirectional NDJSON loop over stdio instead of the one-shot print path:

```
your orchestrator                     caudra --print --input-format stream-json
        │                                             │
        │  {"type":"user",...}            (stdin)     │
        ├─────────────────────────────────────────────►
        │                                             │
        ◄─────────────────────────────────────────────┤
        │  system / assistant / stream_event / result │
        │  one JSON object per line       (stdout)    │
```

Inbound messages (`user`, `control_request`, `control_response`, `control_cancel_request`) drive the agent; outbound messages match the Claude Code SDK shape. Under the hood it reuses the same driver as the TUI and ACP server, so sessions, tools, and tool-call permissions use the same policy. Project MCP startup trust must be approved through `/mcp` in the TUI before a headless run.

SDK-only flags (`--system-prompt`, `--max-turns`, `--session-id`, `--fork-session`, `--permission-mode`, `--include-partial-messages`, ...) are listed in the [CLI flag matrix](/docs/cli/#flags-by-run-path).

Two caveats:

- One-shot `--print` always starts a **new** session in **build** mode, unlike the TUI, which opens in plan mode. Plan mode and session resume need the SDK path (or the TUI).
- The plan file for SDK `--permission-mode plan` is `./plan.md` under cwd, not the state-dir `plans/<slug>.md` files the TUI uses.

### Quick example

```bash
echo '{"type":"user","message":{"content":"explain this repo"}}' \
  | caudra --print --input-format stream-json --max-turns 3
```

## Examples

Pipe compiler errors back for a fix:

```bash
cargo build 2>&1 | caudra --print --yolo --prompt "Fix these compiler errors."
```

Generate a changelog from recent commits:

```bash
git log --oneline v1.2.0..HEAD | caudra --print --prompt "Write a user-facing \
  changelog grouped by: Added, Changed, Fixed. Skip chores."
```

Automated PR summaries in CI:

```bash
SUMMARY=$(git diff main..HEAD | caudra --print --prompt "Write a 2-3 sentence \
  summary of this change for a PR description.")
gh pr edit --body "$SUMMARY"
```

Migrate an API across many files:

```bash
grep -rl 'old_api_call' src/ | while read file; do
  caudra -p --yolo --allowed-tools Read,Edit </dev/null \
    --prompt "In $file, migrate old_api_call() to new_api_call(). Keep behavior identical."
done
```

The `</dev/null` matters. Inside a loop fed by a pipe, Caudra would otherwise read the remaining loop input as prompt text.

Cost tracking:

```bash
caudra -p --output-format json --prompt "refactor the database layer" | jq '.total_cost_usd'
```
