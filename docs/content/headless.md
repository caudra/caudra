---
title: "Headless Mode"
description: "--print for scripts and CI. Drop-in Claude Code compatible."
---

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

A local one-shot `--print` run on Unix can participate in [experimental cross-session messaging](/docs/messaging/) while its main agent is active. Enable `experimental.cross_session_messaging = true` in the global `caudra.toml` before starting it. Every participating process needs the opt-in and a restart after changing the switch. The run answers to a [messaging name](/docs/messaging/#messaging-names) generated for its new session.

Messages enter only at safe boundaries during the active run. Print does not stay alive waiting for peers or start another run after its final result. Held messages have no interactive approval path, and queued or held messages still in memory disappear when the receiving run closes. A `queued` receipt is not a promise that the model will process the message before exit. Use the TUI's `/messages` review when you need interactive approval. A print run never joins a [consumer group](/docs/messaging/#consumer-groups) or takes its work.

To tell running agents about an event from a script or CI job without starting an agent, use [`caudra message`](/docs/cli/#caudra-message).

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
- In embedded mode, the plan file for SDK `--permission-mode plan` is `./plan.md` under cwd rather than the state-dir `projects/<project-id>/plans/<slug>.md` files the TUI uses. Remote plans remain client-owned documents addressed by opaque reference. A `--fork-session` fork starts without a plan.

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

A parent `result` ends that run, not the session's background work. Keep stdin open to receive automatic continuation after reports and outcomes arrive. Each parent run emits a `system` message with subtype `turn_start`. Its `run_id`, `automatic`, `task_event_ids`, `workflow_events`, and [`automation_events`](#automation-runs) also appear under `run` in the matching `result`. The result's `background_active` counts active managed tasks. Permission requests can still arrive from children after the parent result.

The `interrupt` control stops the parent and all session tasks, shell jobs, and workflows and suppresses late automatic continuations. A new user prompt re-enables them. It also [pauses the session's automations](#pause-and-resume) until the next prompt or `automation_resume`, even on an idle session. `automation_pause` pauses them and lets the run go on. Closing stdin cancels and drains owned work. It does not leave a daemon running.

### Goals

A stream-json session can pursue a [completion goal](/docs/commands/#completion-goals), checked by the same evaluator as `/goal` in the TUI. A `/goal` user message reaches the model as plain text, so the client sets and watches the goal through controls. Every stream-json session answers them, and `init` lists them:

```json
{"type":"system","subtype":"init","goal_controls":["goal_set","goal_clear","goal_status"],...}
```

#### Goal controls

Goal controls use the same `control_request` envelope as task and workflow controls, with arguments beside `subtype`:

```json
{"type":"control_request","request_id":"g1",
 "request":{"subtype":"goal_set","condition":"tests pass and cargo clippy is clean","continuation_limit":8}}
```

| Subtype | Arguments | Effect | Reply |
|---------|-----------|--------|-------|
| `goal_set` | `condition`, `continuation_limit` (optional), `kickoff` (optional, default `true`) | Set the goal, replacing the active goal and the last finished one. Unless `kickoff` is `false`, queue the condition as a prompt | The new goal's status |
| `goal_clear` | None | Clear the active goal. A finished goal stays on record | `cleared`: the condition it cleared, or `null` |
| `goal_status` | None | Read the active goal, or the last finished one when none is active | `status`: `active`, `finished`, or `none` |

A successful `control_response` carries the reply in `response.response`. Failures use `response.error`. A status reply always carries the session's `continuation_limit`. Unless `status` is `none`, it also carries the goal under the field names of the [`finished` event](#goal-events). For an active goal, `verdict` and `reason` come from the latest evaluation and are `null` before the first.

The queued prompt is the condition with the kickoff instructions `/goal` adds. It runs like a prompt the client sent, with its own `turn_start` and `result`. With `kickoff: false`, nothing starts, and the goal is evaluated when the next run ends.

`continuation_limit` caps the automatic continuations one run may make. It defaults to 16, and a value above 100 is clamped to 100. The limit belongs to the session, so later goals keep it.

`goal_set` refuses a missing or blank `condition`, or one over 4,000 characters, and `error` gives the reason, such as `goal condition is empty`. A refused request changes nothing.

#### Goal events

Every goal event arrives as a `system` message with subtype `goal`, before the `result` of the run it belongs to. `kind` names the event, and its fields sit beside it:

```json
{"type":"system","subtype":"goal","kind":"evaluation","verdict":"not_met",
 "reason":"two tests still fail","evaluation":2,"applied":true,"cost":0.0054,
 "billing":"api","model":"anthropic/claude-haiku-4-5",...}
```

| `kind` | Fields | When |
|--------|--------|------|
| `evaluating` | `evaluation` | An evaluation starts. `evaluation` numbers it within the goal |
| `evaluation` | `verdict` (`met`, `not_met`, or `impossible`), `reason`, `evaluation`, `applied`, `usage`, `cost`, `billing`, `model` | The evaluator answered |
| `evaluation_failed` | `evaluation`, `message`, `applied`, `usage`, `cost`, `billing`, `model` | The evaluator call failed. The goal stays active |
| `deferred` | `active_background_tasks` | The run ended before session work settled, so evaluation waits. See [Check-ins and resume](#check-ins-and-resume) |
| `finished` | `condition`, `verdict`, `reason`, `evaluations`, `duration_ms`, `usage`, `cost`, `subscription_cost` | The goal was met or judged impossible, and cleared itself |
| `loop_cap` | `evaluations`, `continuations`, `limit` | The run used all its automatic continuations. The goal stays active |
| `turn_limit` | `evaluations` | The run reached its turn limit. The goal stays active |
| `cleared_after_error` | `condition`, `message` | An unrecoverable error ended the run and cleared the goal |

`applied` is `false` when the outcome no longer counts, for example because the goal was replaced while the evaluator ran. `billing` says who pays `cost`: `api` or `subscription`. The `usage`, `cost`, and `subscription_cost` of `finished` total the goal's whole spend, evaluator calls included. As in the `result`, `cost` is money owed and `subscription_cost` the list price a subscription covered.

Evaluator spend also counts in the run's `result`. Unlike one-shot `--print`, an impossible goal, a failed evaluation, or the continuation cap leaves that `result` a success, so read these outcomes from the goal events.

#### Check-ins and resume

A `deferred` goal waits for session tasks, shell jobs, and workflows to settle, so keep stdin open. Results that arrive start an automatic run through the usual [continuation lifecycle](#background-tasks), and the goal is evaluated when that run ends. If the work settles with nothing to deliver, Caudra starts an automatic check-in run instead. Its hidden prompt asks the model to review the work and continue, and the goal is evaluated when it ends.

A prompt that arrives first takes the check-in's place. An `interrupt`, a `set_permission_mode` request, or a run that stops short of a normal end of turn calls the check-in off. That covers an error, a cancellation, and the turn or output-token limit. In each case the goal stays active and is evaluated when the next run ends.

The goal belongs to the session, which saves it after every run and when stdin closes. `--resume <ID>` restores the active goal with its evaluation count, spend, elapsed time, and latest verdict and reason, along with the last finished goal and the continuation limit. `goal_status` reports what came back. A resumed goal waits for the next prompt and is evaluated at the end of that run.

### Automations

Automations are [experimental](/docs/configuration/#experimental-features) and need `experimental.automations`. With it on, a stream-json session runs an automation runtime beside the agent. Its scripts react to session events, such as the session going idle or a schedule coming due, and the messages they queue start runs of their own. The client arms and watches them through controls, and the model can read the same runtime through the [`automation` tool](/docs/tools/#automation).

The `init` message says whether the runtime is attached and which controls it answers:

```json
{"type":"system","subtype":"init","automations":true,
 "automation_controls":["automation_list","automation_validate","automation_arm","automation_disarm",
                        "automation_trust","automation_inspect","automation_history","automation_firing",
                        "automation_dry_run","automation_set_args","automation_set_state",
                        "automation_clear_state","automation_drop","automation_pause","automation_resume"],...}
```

When `experimental.automations` is off or the runtime failed to start, `automations` is `false`, `automation_controls` is empty, and every automation control answers with the `unavailable` error, even a malformed one. One-shot `--print` and the [ACP server](/docs/acp/) attach no runtime.

#### Arming

`--automation` is refused with `--print`, SDK mode included, so the client arms with [`automation_arm`](#automation-controls). As in the TUI, the `automations:` entries of the session's [system prompt profile](/docs/system-prompts/) arm at launch, and so do scripts that declare `arm: "always"`. Bindings are saved with the session. `--resume <ID>` arms them again, and those with an `armed` trigger fire it with reason `resume`.

Project scripts live in `.caudra/automations/<name>.rhai` and user scripts in `~/.config/caudra/automations/`. A session on a remote workspace or a sandbox loads only user scripts. A project script arms only after its content digest has been trusted: take `digest` from its `automation_list` entry and pass it to `automation_trust`. Arming an untrusted script answers with `trust_required`, whose `detail` carries the `name`, `digest`, and `path` to trust. A script that changes on disk gets a new digest and must be trusted again.

An SDK session reports only the `working` and `idle` statuses, so a `needs_input` trigger has nothing to fire on. SDK sessions also take no part in [cross-session messaging](#cross-session-messaging). A script with a `needs_input`, `message_received`, or `work_finished` trigger, or with `meta.messaging` capabilities, is therefore invalid here. `automation_list` lists it with an `availability` of kind `invalid` and a reason such as `meta.triggers[0]: needs_input is unavailable in SDK sessions`. `automation_validate` returns the same reason as its `report`, with `ok` set to `false`, and `automation_arm` answers with the `invalid` error. A saved binding of such a script stays with the session, and the TUI arms it again when it resumes the session.

#### Automation controls

Automation controls use the same `control_request` envelope as task, goal, and workflow controls, with arguments beside `subtype`:

```json
{"type":"control_request","request_id":"a1",
 "request":{"subtype":"automation_arm","name":"deploy-watch","args":{"branch":"main"}}}
```

| Subtype | Arguments | Effect | Reply `kind` |
|---------|-----------|--------|--------------|
| `automation_list` | None | List every script the session sees, including those that cannot load | `automations`: one entry per name |
| `automation_validate` | `name` | Check the header and the args the body reads, then run the body once per trigger on a sample event, performing nothing | `validation`: `name`, `ok`, and a `report` |
| `automation_arm` | `name`, `args` (optional object) | Arm it, or arm it again with new args. Without `args`, the stored args or the defaults apply. The binding records `sdk` as its origin | `automation`: its entry |
| `automation_disarm` | `name` | Disarm it | `automation` |
| `automation_trust` | `name`, `digest` | Trust a project script at this digest | `automation` |
| `automation_inspect` | `name`, `session_id` (optional) | Read its binding, state, and newest firings, in this session or in the one `session_id` names | `detail`: `binding`, `state`, and `firings` |
| `automation_history` | `name`, `fire_id`, `limit` (all optional) | List this session's newest firings, of one automation or all, at most `limit` (default 20, at most 50). With `fire_id`, read that firing of this session | `firings`, or `firing` with `fire_id` |
| `automation_firing` | `fire_id` | Read a firing of any session, with its event, actions, and state patch | `firing` |
| `automation_dry_run` | `fire_id` | Run a finished firing of this session again against the script on disk now, performing and storing nothing. See [Dry runs](/docs/automations/#dry-runs) | `dry_run`: `fire_id`, `trace`, `answers`, `limited`, and `state_revision` |
| `automation_set_args` | `name`, `args` (object) | Replace the args and arm it again | `automation` |
| `automation_set_state` | `name`, `state` (object), `expected_revision` | Replace the state | `state`: the new `revision` |
| `automation_clear_state` | `name`, `expected_revision` | Empty the state | `state` |
| `automation_drop` | `fire_id`, `seq` (optional) | Drop a queued or deferred firing. With `seq`, drop only that message of the firing while it waits for delivery | `ack` |
| `automation_pause` | None | [Pause](#pause-and-resume) every automation of the session. The run, tasks, and workflows go on | `controls`: the pause and the limit counters |
| `automation_resume` | None | Lift the pause without a prompt | `controls` |

An entry carries the script's `availability`, whose `kind` is `armed`, `available`, `needs_trust`, or `invalid` with a `reason`. Its `armed` field holds the origin of the arming, such as `sdk` or `profile`, and is `null` while the automation is not armed. State writes apply only at the revision you read: take `expected_revision` from the `state.revision` that `automation_inspect` returns. A `null` optional argument counts as absent.

A `dry_run` reply carries the replayed `fire_id` and the run as a `trace`, in the shape `automation_firing` returns. The trace's `firing.fire_id` is `dry-run`, and its `firing.digest` is the digest of the script that ran. `answers` holds one entry per row of `trace.actions`: `recorded` for an action recorded instead of performed, `journal` or `stubbed` for an `http()` request answered from the original firing's journal or with the stub, and `cut` when that journaled result was too large to store. `limited` is the automation limit that would have refused a real firing, with its `reason` (`cooldown`, `max_per_hour`, or `backoff`) and `until`, or `null`. `state_revision` is the revision of the state copy that `trace.state_patch` applies to. A dry run needs no trust, so an untrusted edit of a project script runs too.

The runtime runs one dry run at a time. Another `automation_dry_run` waits until the running one ends, and each can take up to 120 seconds, the [wall-time limit](/docs/automations/#limits-and-safety) of a firing. Other controls are answered meanwhile. Closing stdin does not wait for dry runs. When the runtime stops, the running one and those waiting behind it answer `unavailable`, and so does a request that reaches the runtime after it began to stop.

The answer lands in a `control_response` under `response.response.automation`, as a `kind` / `detail` pair:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"a1",
 "response":{"automation":{"kind":"automation","detail":{"name":"deploy-watch",
   "availability":{"kind":"armed"},"armed":"sdk","args":{"branch":"main"},...}}}}}
```

A refused request is an `error` response. `error` carries the message and `response.automation_error` the structured cause:

```json
{"type":"control_response","response":{"subtype":"error","request_id":"a2",
 "error":"unknown automation firing \"3vQBu5nX2kYmR8tL7eHcWd\"",
 "response":{"automation_error":{"kind":"unknown_firing","detail":{"fire_id":"3vQBu5nX2kYmR8tL7eHcWd"}}}}}
```

| Error `kind` | `detail` | Cause |
|--------------|----------|-------|
| `unavailable` | None | No runtime serves the session, or it stopped |
| `unknown_automation` | `name` | No script has this name |
| `trust_required` | `name`, `digest`, `path` | The project script's digest is not trusted |
| `invalid` | `name`, `reason` | The script cannot load here, or the request does not fit it |
| `args` | `name`, `reason` | The args do not fit what the script declares |
| `state_conflict` | `name`, `current` | The state moved past `expected_revision`. `current` is its revision now |
| `unknown_firing` | `fire_id` | No firing has this id. `automation_history` looks only in this session, and `automation_dry_run` replays only this session's firings |
| `not_replayable` | `fire_id`, `reason` | The firing cannot run again: it has not finished, its event was too large to keep or no longer reads as an event, or the current script has no trigger for its event. `reason` says which, as in `it is still queued, deferred or running` |
| `not_waiting` | `fire_id`, and `seq` when given | The firing or message already ran, was delivered, or was dropped |
| `session_not_saved` | `session_id` | The session has no saved record yet |
| `storage` | The message | The automation store failed |
| `internal` | The message | The runtime failed, or `session_id` is not a session id |

A control that lacks a required argument, or carries one of another type, is refused before it reaches the runtime. `error` names the control and the argument, as in `automation_trust requires a string digest`, and no `automation_error` comes with it.

The texts of `unknown_automation` and `trust_required` end with the next step, such as `review the script, then trust this digest`. A client takes it with `automation_list` or `automation_trust`.

#### Firings and notices

Firings and notices share the ordered stdout stream with agent messages, and those sent before the session ends are written before Caudra exits. Every firing that has ended arrives once, as a `system` message with subtype `automation_fired` and the firing's fields beside it:

```json
{"type":"system","subtype":"automation_fired","fire_id":"3vQBu5nX2kYmR8tL7eHcWd",
 "automation":"deploy-watch","trigger":"schedule","trigger_index":0,"status":"completed",
 "reason":null,"error":null,"queued_at":1790000000000,"started_at":1790000000004,
 "finished_at":1790000000011,"action_count":1,"first_action":"message",...}
```

| Field | Meaning |
|-------|---------|
| `fire_id`, `automation`, `digest` | The firing, its automation, and the digest of the script version that ran |
| `trigger`, `trigger_index` | The trigger kind and its position in the script's `meta.triggers` |
| `event_key` | The key of a keyed event, such as a finished workflow run, or `null` |
| `consumed` | Whether the firing took a peer message for itself. Always `false` here |
| `status` | `completed`, `skipped`, `released`, `failed`, `rate_limited`, `cancelled`, `paused`, `dropped`, or `interrupted` |
| `reason` | The reason for an outcome such as a skip or a pause, or `null` |
| `error` | How it failed or was stopped, with `kind`, `message`, and the script `line` and `column` when known, or `null` |
| `repeats` | How many firings this report stands for, counting the quiet skips it absorbed |
| `attempts` | How often a limit deferred it |
| `operations` | How many script operations it ran |
| `state_outcome` | `committed` when its state change landed, `conflict` when a newer revision won, or `null` without a change |
| `queued_at`, `deferred_until`, `started_at`, `finished_at` | Unix time in milliseconds. Only `queued_at` is never `null` |
| `action_count`, `first_action` | How many actions it took, and the kind of the first, such as `message` or `notify`, or `null` |
| `absorbed` | Present only when the firing replaced an earlier quiet skip. It names that skip's `fire_id`, whose report no longer stands, and `repeats` counts it |

A `system` message with subtype `automation_notice` carries text for the client to show:

```json
{"type":"system","subtype":"automation_notice","automation":"deploy-watch",
 "fire_id":"3vQBu5nX2kYmR8tL7eHcWd","text":"deploy finished"}
```

`text` is what a script passed to `notify()`, or why the session refused a goal or an arming. A goal that a script's `set_goal()` requested while another goal is active gets `a goal is active or already queued; pass replace: true to replace it`, and the run starts with the active goal unchanged. `fire_id` names the firing the notice came from. It is absent when an arming at launch or on resume failed, such as the saved binding of a script that is invalid here.

Caudra keeps up to 1,024 automation updates waiting for stdout and drops the oldest past that, so read missed firings back with `automation_history`.

#### Automation runs

Messages that automations queue start automatic runs, each with its own `turn_start` and `result`. Such a run can start before the first prompt. It then uses the session's current permission mode, thinking level, and fast mode, and later automatic runs keep those settings until a prompt replaces them. Task and workflow results that are still waiting join that run.

A queued `user` message runs before any automation message. A message a script sends with `delivery: "guide"` joins a run already going, but while a prompt is queued it waits and then joins that prompt's run.

`turn_start`, and `run` in the `result`, list the automation messages a run took as `automation_events`, beside `task_event_ids` and `workflow_events`:

```json
{"type":"system","subtype":"turn_start","run_id":3,"automatic":true,"task_event_ids":[],"workflow_events":[],
 "automation_events":[{"automation":"deploy-watch","fire_id":"3vQBu5nX2kYmR8tL7eHcWd","seq":0}],...}
```

Each entry names the automation, the firing, and the message's `seq` among that firing's actions, the `seq` that `automation_drop` takes. `turn_start` lists the messages the run started with. The `result` adds the `guide` messages that joined it, in order and without duplicates.

Automation runs bill like any other run, and the [`[automations]`](/docs/configuration/#automations) limits and each script's own limits hold as in the TUI.

#### Pause and resume

The `interrupt` control pauses every automation of the session, even when the session is idle. `automation_pause` sets the same pause and leaves the run, tasks, and workflows going. Running firings end as `cancelled`. Events that were waiting, and those that arrive during the pause, end as `paused` firings with the pause's reason, such as `paused by the SDK client`. Messages already queued wait for delivery. A script can set the same pause with `pause_automations()`. If the automations are paused already, the pause keeps its first reason and source.

The next `user` message lifts the pause, and so does a `goal_set` kickoff, since both are human input. Human input also resets the turn count that `max_unattended_turns` caps and ends the delivery backoff that follows a failed automation run. `automation_resume` lifts the pause without a prompt and leaves that turn count and the delivery backoff as they are. Once the pause lifts, armed automations with an `armed` trigger fire it with reason `unpaused`. Without a pause, `automation_resume` changes nothing.

Both controls answer with `controls`. Its `controls.pause` holds the pause with its `reason`, its `source`, and the time `at` it was set, and is absent once the pause lifts:

```json
{"type":"control_response","response":{"subtype":"success","request_id":"p1",
 "response":{"automation":{"kind":"controls","detail":{"controls":{
   "pause":{"reason":"paused by the SDK client","source":{"kind":"sdk"},"at":1790000000000},
   "turn_window":{"turns":[]},"unattended":{"count":0},"delivery_backoff":{"errors":0,"until":null}},
   "turns_per_hour":20,"max_unattended_turns":null,"blockers":[]}}}}}
```

`unattended.count` is the turn count that `max_unattended_turns` caps. `delivery_backoff.errors` counts the automation runs that ended in error since the last clean run or human input, and `delivery_backoff.until` is when the next delivery may go. `turn_window.turns` holds the start times of the automation turns in the rolling hour that `turns_per_hour` caps, and `blockers` lists what keeps the session from settling.

Closing stdin never pauses automations. Once it closes, no automation run starts. Automations stop before the rest of the session's work, and firings still running end as `interrupted`. The session is then saved with any pause and its limit counters, and `--resume <ID>` restores both, so a session closed while paused stays paused until the next human input or `automation_resume`.

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
