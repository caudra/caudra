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
- The plan file for SDK `--permission-mode plan` is `./plan.md` under cwd rather than the state-dir `projects/<project-id>/plans/<slug>.md` files the TUI uses.

### Quick example

```bash
echo '{"type":"user","message":{"content":"explain this repo"}}' \
  | caudra --print --input-format stream-json --max-turns 3
```

### Workflows

A stream-json session runs a workflow runtime beside the agent. Scripts under `.caudra/workflows/<name>.rhai` in the project can be started, watched, paused, and resumed from the wire, and the model can reach the same runtime through the [`workflow` tool](/docs/tools/#workflow). Runs outlive individual turns and are journaled, so a session resumed with `--resume` can pick a run up where it stopped.

The `init` message says whether the runtime is attached and which controls it answers:

```json
{"type":"system","subtype":"init","workflows":true,
 "workflow_controls":["workflow_list","workflow_validate","workflow_start","workflow_status",
                      "workflow_pause","workflow_resume","workflow_stop","workflow_trust","workflow_ack"],...}
```

When the runtime failed to start, `workflows` is `false`, `workflow_controls` is empty, and every workflow control answers with the `unavailable` error. One-shot `--print` and the [ACP server](/docs/acp/) attach no runtime.

#### Controls

A workflow control is a `control_request` whose `subtype` is one of the advertised names. Its arguments sit beside `subtype`:

```json
{"type":"control_request","request_id":"r1",
 "request":{"subtype":"workflow_start","name":"review","args":{"branch":"main"},"agent_budget":8}}
```

| Subtype | Arguments | Answer `kind` |
|---------|-----------|---------------|
| `workflow_list` | | `catalog`: `entries` (with `name`, `digest`, `trusted`, `source_kind`, `phases`), `invalid`, and the `project_dir` and `user_dir` a new script would go in |
| `workflow_validate` | `name` | `validation`: `ok` and a `report` |
| `workflow_start` | `name`, `args` (object, default `{}`), `agent_budget` | `started`: the new run |
| `workflow_status` | `run_id` (optional) | `runs` for every run, `run` for one |
| `workflow_pause` | `run_id` | `run`, once its agents have stopped |
| `workflow_resume` | `run_id`, `agent_budget` (optional) | `run` |
| `workflow_stop` | `run_id` | `run`, once its agents have stopped |
| `workflow_trust` | `name`, `digest` | `trusted` |
| `workflow_ack` | `run_id`, `revision` | `acked`: `true` when the notice was still pending |

The answer lands in a `control_response` under `response.response.workflow`, as a `kind` / `detail` pair:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"r1",
 "response":{"workflow":{"kind":"started","detail":{"run_id":"run-1","status":"active",...}}}}}
```

A refused request is an `error` response. `error` carries the message and `response.workflow_error` the structured cause:

```json
{"type":"control_response","response":{"subtype":"error","request_id":"r2",
 "error":"unknown workflow run \"run-9\"",
 "response":{"workflow_error":{"kind":"unknown_run","detail":{"run_id":"run-9"}}}}}
```

A control that lacks a required argument, such as `workflow_stop` without `run_id`, is refused the same way before it reaches the runtime.

Pause and stop wait for the run's agents to finish stopping, and permission requests keep flowing in the meantime. Answer them as usual.

#### Trust

A project workflow runs only after its content digest has been trusted. Take the digest from `workflow_list` and pass it to `workflow_trust`. Starting an untrusted script answers with `trust_required`, whose `detail` carries the `name`, `digest`, and `path` to trust. A script that changes on disk gets a new digest and must be trusted again.

#### Run events

Every change to a run arrives as a `system` message with subtype `workflow`. `event.kind` is `snapshot` for a new run state and `log` for a line of run output. The `workflow_*` keys identify the run, its execution epoch, and the phase that produced the event:

```json
{"type":"system","subtype":"workflow",
 "event":{"kind":"snapshot","run_id":"run-1","status":"active","phase":"gather","revision":3,...},
 "workflow_run_id":"run-1","workflow_epoch":1,"workflow_call_key":0,"workflow_phase":"gather"}
```

Agents a workflow launches stream as subagents. Their `assistant` and `user` messages carry the same `workflow_*` keys beside `parent_tool_use_id`, so a client can group them by run.

#### Completion context

A finished, failed, or paused run leaves a completion notice. Caudra prepends the pending notices to the content of the next `user` message, one block per run, and acknowledges each one so it is delivered once per `(run_id, revision)`. There is no automatic model turn: the report reaches the model with your next prompt.

```
Workflow review (review) finished with status completed.
Report: Two findings, both in src/auth.rs ...
Scratch file: /tmp/caudra/review/run-1.md

<your prompt>
```

`Report:` is the `report` string of the run's result. Without one, the whole result is inlined as `Result:`. Either is cut at 8 KiB. A paused run adds `Paused:` with its message and a failed run adds `Error:`.

Send `workflow_ack` yourself only when you handle a notice from a `snapshot` event directly and do not want it in the next prompt.

#### Restart and resume

Closing stdin marks every active run `interrupted` before the session saves, and the same happens when the process dies. Resume the session with `--resume <ID>` and call `workflow_resume` with the run id: the run replays its journal and continues from the last committed phase. A resume is at-least-once. Work that an agent had started but not committed runs again.

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
