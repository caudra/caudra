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

## Cross-session messaging

A local one-shot `--print` run on Unix can participate in [experimental cross-session messaging](/docs/sessions/#cross-session-messaging) while its main agent is active. Enable `experimental.cross_session_messaging = true` in the global `caudra.toml` before starting it. Every participating process needs the opt-in and a restart after changing the switch.

Messages enter only at safe boundaries during the active run. Print does not stay alive waiting for peers or start another run after its final result. Held messages have no interactive approval path, and queued or held messages still in memory disappear when the receiving run closes. A `queued` receipt is not a promise that the model will process the message before exit. Use the TUI's `/messages` review when you need interactive approval.

The same [inbound policy, same-user trust assumption, and provider exposure](/docs/permissions/#cross-session-messages) apply as in the TUI. Persistent SDK stream sessions and ACP do not participate in this MVP. Neither do remote Workcell or managed sandbox sessions. Selecting `--output-format stream-json` alone does not select the SDK path, but `--input-format stream-json` does.

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

Auto mode needs `experimental.decision_engine`. Without it, `--permission-mode auto` stops Caudra with an error, and a `set_permission_mode` control request for `auto` returns that error and leaves the mode unchanged. See [SDK permission modes](/docs/cli/#permission-modes-sdk).

Two caveats:

- One-shot `--print` always starts a **new** session in **build** mode, unlike the TUI, which opens in plan mode. Plan mode and session resume need the SDK path (or the TUI).
- In embedded mode, the plan file for SDK `--permission-mode plan` is `./plan.md` under cwd rather than the state-dir `projects/<project-id>/plans/<slug>.md` files the TUI uses. Remote plans remain client-owned documents addressed by opaque reference.

With a Workcell selector, SDK stream sessions handle text-only `/remote` recovery commands and standalone `cd` messages before model dispatch. Recovery responses have system subtype `remote` or `remote_error`. Directory changes return `cwd` or `cwd_error`. See [Remote Workspaces](/docs/remote-workspaces/#recovery-commands) for exact commands, acknowledgement requirements, and the no-retry policy. For recovery without a model session, use [`caudra remote`](/docs/cli/#caudra-remote).

### Quick example

```bash
echo '{"type":"user","message":{"content":"explain this repo"}}' \
  | caudra --print --input-format stream-json --max-turns 3
```

### Background tasks

Persistent stream-JSON sessions support [background tasks and shell jobs](/docs/sessions/#background-tasks). The `init` message advertises `background_jobs`, `background_tasks`, `background_shell`, `job_kinds`, and `task_controls`. The `task_execution` and `shell_execution` objects include configured and effective policies. `shell_execution.async_threshold_secs` gives the requested-timeout threshold.

One-shot `--print`, including `--output-format stream-json` without stream input, and ACP resolve `auto` to synchronous execution. Strict `async` has no effective policy there: the affected tool is withheld and stale calls fail with an actionable error. See [execution policies](/docs/sessions/#execution-policies) for defaults and timeout boundaries.

Task controls use the same `control_request` envelope as workflow controls:

```json
{"type":"control_request","request_id":"t1","request":{"subtype":"task_status","task_id":"implement-active-footer-chips"}}
```

| Subtype | Arguments | Effect |
|---------|-----------|--------|
| `task_list` | None | List session-owned agent and shell jobs |
| `task_status` | `task_id` | Return one task's status |
| `task_cancel` | `task_id` | Cancel one task |
| `task_promote` | `task_id` | Promote an agent task without restarting it, in task `auto` mode |

A successful `control_response` carries the task list or status in `response.response`. Failures use `response.error`.

Status `kind` distinguishes `agent` from `shell`. Shell entries carry command metadata and have no child transcript. They support inspection and cancellation, but cannot be promoted or resumed with `task`. Child-owned command results return to that exact child invocation rather than starting a main-agent continuation.

New task IDs come from the description. Shell jobs use safe command labels such as `shell-cargo-test`, falling back to `shell`. Collisions add numeric suffixes such as `-2`. Pass the returned ID unchanged. Older IDs remain valid. See [task and output IDs](/docs/sessions/#task-and-output-ids).

Task status `result` is now a native JSON outcome object, replacing the JSON-encoded string. Read `result.output` directly, preserving its object, array, scalar, or string type. Do not JSON-decode `result` a second time. This corrects the SDK wire shape without rewriting saved outcomes.

If an outcome exceeds the status limit, `result` is omitted, `result_truncated` is `true`, and `result_preview` contains bounded text. The preview may be incomplete JSON and is not a complete result. The full outcome is retained in the session's output store. When `output_ref` is present, pass its `id` as `output_id` to `tool_output` to read or search that outcome. New output handles identify the producer, such as `output-file-grep`, with numeric suffixes for collisions. Older handles remain valid. Model-facing status and terminal notices include retrieval guidance when their result is incomplete. Complete short results do not prompt another fetch.

`reports_truncated` marks shortened or omitted report text. Task lists omit result and report details to stay lightweight. Request `task_status` for details and the retained output reference.

A parent `result` ends that run, not the session's background work. Keep stdin open to receive automatic continuation after reports and outcomes arrive. Each parent run emits a `system` message with subtype `turn_start`. Its `run_id`, `automatic`, `task_event_ids`, and `workflow_events` also appear under `run` in the matching `result`. The result's `background_active` counts active managed tasks. Permission requests can still arrive from children after the parent result.

The `interrupt` control stops the parent and all session tasks, shell jobs, and workflows and suppresses late automatic continuations. A new user prompt re-enables them. Closing stdin cancels and drains owned work. It does not leave a daemon running.

### Workflows

Workflows are [experimental](/docs/configuration/#experimental-features) and need `experimental.workflows`. With it on, a stream-json session runs a workflow runtime beside the agent. Scripts under `.caudra/workflows/<name>.rhai` in the project can be started, watched, paused, and resumed from the wire, and the model can reach the same runtime through the [`workflow` tool](/docs/tools/#workflow). Runs outlive individual turns and keep a journal. Pause a run before closing the session if you intend to resume it later.

The `init` message says whether the runtime is attached and which controls it answers:

```json
{"type":"system","subtype":"init","workflows":true,
 "workflow_controls":["workflow_list","workflow_validate","workflow_start","workflow_status",
                      "workflow_pause","workflow_resume","workflow_stop","workflow_trust","workflow_ack"],...}
```

When `experimental.workflows` is off or the runtime failed to start, `workflows` is `false`, `workflow_controls` is empty, and every workflow control answers with the `unavailable` error. One-shot `--print` and the [ACP server](/docs/acp/) attach no runtime.

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
| `workflow_inspect` | `run_id` | `detail`: the `run`, its journal as `calls`, its timeline as `events`, and `journal_trimmed` |
| `workflow_history` | `limit` (optional, default 20) | `history`: runs of other sessions, newest first, each with `session_id` and `session_title` |
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
 "event":{"kind":"snapshot","run_id":"run-1","status":"active","phase":"gather","revision":3,
          "phase_history":[{"title":"gather","started_at":1730000000}],
          "logs":[{"at":1730000012,"message":"3 sources found"}],...},
 "workflow_run_id":"run-1","workflow_epoch":1,"workflow_call_key":0,"workflow_phase":"gather"}
```

A snapshot carries `phase_history`, every phase the run entered with its start time in seconds since the epoch, and `logs`, the last 200 lines with the same stamp. A `log` event carries one line with `at`, `message`, and the `revision` it belongs to. `workflow_inspect` returns the stored timeline in full as `events`, each with a `seq`, `at`, `kind` of `phase` or `log`, and `text`.

Agents a workflow launches stream as subagents. Their `assistant` and `user` messages carry the same `workflow_*` keys beside `parent_tool_use_id`, so a client can group them by run.

#### Completion context

A finished, failed, or paused run leaves a completion notice. Caudra delivers pending notices as context at the next safe parent-run boundary and acknowledges delivery per `(run_id, revision)`. If the parent has already answered, the SDK starts an automatic run without another user prompt, using the same [continuation lifecycle](#background-tasks) as background tasks.

```
Workflow review (review) finished with status completed.
Report: Two findings, both in src/auth.rs ...
Scratch file: /tmp/caudra/review/run-1.md
```

`Report:` is the `report` string of the run's result. Without one, the whole result is inlined as `Result:`. Either is cut at 8 KiB. A paused run adds `Paused:` with its message and a failed run adds `Error:`.

Send `workflow_ack` yourself only when you handle a pending notice from a `snapshot` event directly and do not want the model to consume it. Delivery can already have started by the time your acknowledgement arrives.

#### Restart and resume

Closing stdin stops and drains session-owned work before saving. Workflow runs left active at shutdown become `interrupted` and cannot resume. Pause a run first, then reopen the session with `--resume <ID>` and use `workflow_resume`. See [How resume works](/docs/workflows/#how-resume-works) for journal replay and its at-least-once effects.

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
