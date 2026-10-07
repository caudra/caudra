---
name: caudra-automation-dev
description: Write, validate, arm, and debug Caudra automations, the Rhai scripts whose triggers react to session events such as idling, a finished goal, a message from another session, a finished workflow run or a schedule, and whose actions go through a small host ABI.
---

# Writing Caudra automations

This is the complete reference for authoring automations. It stands on its own, so you do not need the user docs to write a working script.

## 1. What an automation is

An automation is a short Rhai script that reacts to events in one session. Its header lists triggers, such as the session going idle, a goal finishing, a message arriving from another session, a workflow run finishing, or a schedule coming due. When a trigger matches, the body runs once with the event in scope. That run is a firing. A firing decides whether to act, and it acts only through a few host functions: it queues a message for the model, sets a goal, notifies the user, calls an HTTP endpoint its header declares, messages other sessions, starts a workflow its header declares, writes a log line, or pauses every automation in the session.

The script is trusted. Values from outside it, such as the model's last response, the session title, or another session's message, arrive marked untrusted, and the host refuses to turn them into this session's instructions.

An automation does nothing until the user arms it in a session: with `/automations`, with `--automation` at launch, from a prompt profile, or through `arm: "always"` in a user script. Arming takes args, so one script can serve many sessions. An armed automation runs while its session is open and never finishes on its own. Automations run in TUI sessions and in SDK sessions (`--print --input-format stream-json`), whose client arms them with the `automation_arm` control. An SDK session shows no prompt to wait on and takes no part in cross-session messaging, so a script with a `needs_input`, `message_received`, or `work_finished` trigger, or with `messaging` capabilities in its header, is invalid there and cannot be armed. One-shot `--print` and ACP sessions cannot arm automations.

Write an automation when:

- The session should keep working without someone typing, through a backlog or a list of goals.
- Something should happen on a schedule, or when the session has waited on a prompt for too long.
- The session should guard itself, for example by pausing once it has spent too much.
- The session should answer or announce things to other sessions, or follow the work it handed to a consumer group.
- A workflow should run on a schedule or on request, and its result should go somewhere.

Write a workflow instead when the job is one multi-agent plan that runs to a result, such as a review or a research routine. A workflow fans work out to subagents and finishes. An automation reacts to the session, one firing at a time, for as long as it stays armed. The two combine: an automation can start a workflow with `start_workflow()` and act on its result with a `workflow_finished` trigger, as `nightly-review` in section 12 does. For a single objective in one session, a goal is enough: `/goal` keeps the session working until its condition is met.

## 2. Where the file goes

| Scope | Location | Trust |
|-------|----------|-------|
| Project | `<project root>/.caudra/automations/<name>.rhai` | The user must trust the file's exact digest in `/automations`. Changing any byte revokes it. |
| User | `<config dir>/automations/<name>.rhai` | Trusted as written |

Call the `automation` tool with `action: "list"` before writing. Its answer names both directories as resolved for this machine and build. Use those paths rather than guessing. The user directory is `~/.config/caudra/automations/` on Linux and macOS (`XDG_CONFIG_HOME` is honoured) and `%APPDATA%\caudra\automations\` on Windows, with `caudra-debug` in place of `caudra` for a debug build.

Choosing a scope:

- The automation is about this repository, its paths, commands, or conventions: project scope. The user must trust it in `/automations` before it can be armed, and again after every edit. You cannot trust it. Neither can the script.
- The automation is a personal routine for any session: user scope. It can be armed as soon as the file is saved. Only user scripts may use `arm: "always"`.
- When both scopes hold a script with the same name, the project script wins and hides the user script.

File rules:

- The file name without `.rhai` equals `meta.name`.
- A regular file, not a symlink, UTF-8, at most 64 KiB.
- The first statement is the header, `let meta = #{ … };`. Comment lines above it are fine. Anything else above it is not.

A file that breaks a rule stays in `list` as invalid, with the reason. Caudra reads both directories again on every list, every arm, and every session start, so a fixed file shows up without a restart.

## 3. Anatomy of a script

```rhai
let meta = #{
    name: "run-the-tests",
    description: "Ask for a test run when the final response does not mention the test command",
    triggers: [#{ kind: "idle" }],
    limits: #{ max_per_hour: 4 },
    args: #{
        command: #{ type: "string", default_value: "just test", description: "The test command" },
    },
};
if event.started_by.kind == "user" { state.asked = 0; }
if event.outcome != "completed" { skip("the turn ended " + event.outcome); }
if event.last_response.contains(args.command) { skip("the response mentions " + args.command); }
let asked = state.asked ?? 0;
if asked >= 2 { skip("asked twice since the last turn a person started"); }
state.asked = asked + 1;
message("Run " + args.command + " and fix any failures before you finish.");
```

The header comes first and is read without running the script. The body is ordinary Rhai that runs top to bottom once per firing. `return` ends a firing early, and `skip(reason)` ends it and records why. A skip still keeps the firing's `state` changes, so the reset on the first line of the body survives the skips after it.

These names are in scope:

| Name | What it holds |
|------|---------------|
| `event` | The event that fired the script, as a read-only map. Section 5 lists its fields. |
| `state` | The automation's memory in this session, as a map. Section 8 explains when changes are kept. |
| `args` | The values the user gave when arming, with every declared arg present and defaults filled in |
| `meta` | The header, for example `meta.name` |

## 4. Header reference

The header is read without running the script, so it holds only literals: strings, integers, floats, bools, arrays, and maps. Variables, calls, and expressions are refused, even `"a" + "b"`. A key the header does not know is an error.

| Key | Rule |
|-----|------|
| `name` | Required. Kebab-case (`^[a-z0-9]+(-[a-z0-9]+)*$`), at most 64 bytes, and equal to the file name. |
| `description` | Required, at most 512 bytes. Shown in `list` and in `/automations`. |
| `triggers` | Required, 1 to 8 trigger maps. Any one of them fires the script. |
| `limits` | Optional `#{ cooldown, max_per_hour }` |
| `args` | Optional arg specs |
| `network` | Optional list of at most 16 origins that `http()` may reach, such as `"https://hooks.example.com"`. An origin is a scheme and a host, with an optional port and no path. `http://` is allowed only for a loopback or private host. |
| `secrets` | Optional list of at most 16 environment variables that `http()` may read, such as `"GITHUB_TOKEN"`. Names use uppercase letters, digits, and underscores. |
| `messaging` | Optional `#{ reply, send, publish }`: the messages the body may send to other sessions. See Messaging below. |
| `workflows` | Optional list of at most 16 workflow names that `start_workflow()` may launch, such as `"review-changes"`. Names are kebab-case, at most 64 bytes. See Workflows below. |
| `timezone` | Optional IANA zone such as `"Europe/Berlin"`, used by `now()` and schedules. Defaults to the system zone. |
| `arm` | `"manual"` (default) or `"always"`. With `"always"`, every session arms the automation when it starts. Allowed only in user scope, and only when every arg has a `default_value`. |

Header keys avoid Rhai keywords, because a map literal accepts a keyword key only when it is quoted. `#{ default: 1 }` and `#{ for: 1 }` fail to parse. That is why the trigger delay is `after` and an arg default is `default_value`. The maps your body builds follow the same rule.

### Triggers

Each trigger is a map with a `kind`. Durations are strings such as `"30s"`, `"2m"`, `"1h"`, or `"1h 30m"`.

| Kind | Options | Fires when |
|------|---------|------------|
| `armed` | none | the automation is armed: at launch, on resume, when the user arms it again, and when the pause latch clears |
| `idle` | `after`: default `"0s"`, at most `"24h"` | the session settles after a busy period and stays settled for `after` |
| `needs_input` | `after`: as for `idle`. `inputs`: default `["permission", "question", "plan", "auth", "plugin"]` | the session has waited on a person for one of `inputs` for `after` |
| `goal_finished` | `verdicts`: default `["met", "impossible", "cleared"]` | the session goal ends with one of `verdicts` |
| `message_received` | `audiences`, `topics`, `senders`, `scripts`, `admissions`, `from_automations`, and `consume`, under Messaging below | a message from another session or a script reaches this session |
| `work_finished` | `groups`: every group by default. `states`: default `["completed", "failed", "cancelled"]`, and `"paused"` may join them | an item of consumer-group work that this session published reaches one of `states` |
| `workflow_finished` | `workflows`: every workflow by default. `statuses`: default `["completed", "failed", "cancelled", "interrupted"]` | a workflow run in this session reaches one of `statuses`, under Workflows below |
| `schedule` | `every` or `at`, plus `weekdays` and `catch_up` | an occurrence is due |

- `idle` and `needs_input` fire once per transition. The opposite edge cancels a pending `after` timer, so `after: "2m"` waits for two quiet minutes.
- The `needs_input` kinds are a tool `permission` prompt, the `question` form, a ready `plan` awaiting approval, an `auth` prompt, and a `plugin` float. Add `"messages"` to `inputs` to fire while messages from other sessions wait for the user's review.
- `goal_finished` with `cleared` means an error cleared the goal. A goal that runs out of continuations or reaches the turn limit stays active and does not fire.
- A schedule takes exactly one of `every` and `at`. `every` is a period of at least `"1m"`, counted from the moment the automation was armed. `at` is `"HH:MM"` on a 24-hour clock in `meta.timezone`, and `weekdays`, allowed only with `at`, limits it to days such as `["mon", "tue", "wed", "thu", "fri"]`.
- Missed occurrences, for example while the session was closed, collapse into the latest one. Within a minute of its time it fires as usual. Later than that, `catch_up: "once"` (the default) fires it late, and `catch_up: "skip"` drops it.

### Messaging

Sessions on this machine reach each other by messaging name, such as `@worker-1`, by topic, such as `swarm.status`, and by broadcast. A consumer group turns each message on its topics into one work item for one of its members. Messaging triggers and capabilities need cross-session messaging, which only the user can turn on, in the global `caudra.toml`, followed by a restart:

```toml
[experimental]
cross_session_messaging = true
```

Without it, `list` shows a script with a `message_received` or `work_finished` trigger, or with `meta.messaging`, as invalid: `needs experimental.cross_session_messaging`.

`meta.messaging` declares what the body may send. A call outside it stops the firing.

| Key | Allows |
|-----|--------|
| `reply` | `true` lets `reply()` answer the sender of a message this automation consumed |
| `send` | The `@name`s `send()` may reach: `"@lead"`, `"@worker-*"` for every name that starts with `@worker-`, or `"*"` for any |
| `publish` | The concrete topics `publish()` may use, such as `"swarm.status"`, and `"broadcast"` for `broadcast()` |

`message_received` takes these options:

| Option | Default | Rule |
|--------|---------|------|
| `audiences` | `["direct", "topic", "broadcast"]` | `"direct"`: sent to this session's `@name`. `"topic"`: on a topic it subscribes to. `"broadcast"`: to every session that receives broadcasts. |
| `topics` | any topic | Topic patterns. A message without a topic never matches. |
| `senders` | any sender | Sessions by `@name`: `"@lead"`, `"@worker-*"`, or `"*"` |
| `scripts` | any sender | Scripts by the `--from` label they send with through `caudra message`, such as `"nightly-ci"` |
| `admissions` | `["queued"]` | `"queued"`: on its way to the model. `"held"`: waiting for the user's review. |
| `from_automations` | `false` | `true` also fires on messages that other sessions' automations sent |
| `consume` | `false` | `true` takes the message from the model, as described below. Needs `admissions: ["queued"]`. |

Lists here and in `meta.messaging` hold at most 16 entries, without repeats.

- A trigger sees only the messages that reach the session. The session's name, topic subscriptions, broadcasts, and groups belong to the user, who sets them at launch with `--name`, `--topic`, `--receive-broadcasts`, and `--group`, or later with `/topics` and `/groups`.
- A topic pattern is a dotted topic in which `*` matches exactly one segment and a final `**` matches one or more, so `ci.*` matches `ci.failures` and `ci.**` also matches `ci.failures.linux`.
- With `senders` or `scripts` set, the sender must match one of them: a session by `senders`, a script by `scripts`. `senders: ["*"]` alone leaves scripts out.
- The default `auto` inbound policy holds every script message for review, so a `"queued"` trigger sees one only after the user approves it, or under the `accept` policy.
- Each message reaches an automation as one event, even when catch-up brings a topic message back after a restart.
- Group work never reaches `message_received`. Follow it with `idle.work`, `session.work`, and `work_finished`.

**Consumption.** A trigger with `consume: true` takes each message it matches from the model and hands it to this automation alone.

- When several automations would consume a message, the one whose name sorts first takes it, and the others see it with `consumed: false`.
- The message stays with the automation while its event waits, through deferral and restarts.
- It goes back to normal delivery, so the model receives it, when the firing calls `release()`, fails, or is stopped. The same happens when its event is dropped, because the queue overflowed or the event no longer matches after a restart, and when the event is paused or the automation disarmed. A firing that shutdown interrupts releases its message at the next start.
- A firing that completes without `reply()` or `release()`, by running to its end, returning, or skipping, keeps the message from the model for good.
- Release admits the message again under the session's current inbound policy, so it may wait for the user's review.
- While automations are paused, no trigger consumes anything.

**Work outcomes.** `work_finished` follows the consumer-group work this session published, from its model and its automations alike.

- It fires only in the publishing session. Work from a script's publication has no such session, so it never fires.
- The session checks for changes when the message history changes, at most every 5 seconds. An item fires once for the state it is in at a check. An item that changes state twice between checks fires once, with its latest state, so a pause resolved within seconds may never fire.
- It watches from the moment the automation is armed, so older outcomes never fire, and a restart never repeats one.

### Workflows

An automation can start workflows and react when their runs finish. The `workflow_finished` trigger, `meta.workflows`, and `start_workflow()` need workflows, which only the user can turn on, in the global `caudra.toml`, followed by a restart:

```toml
[experimental]
workflows = true
```

Without it, `list` shows a script with a `workflow_finished` trigger, or with `meta.workflows`, as invalid: `needs experimental.workflows`.

`meta.workflows` declares the workflows the body may start, by workflow name. Starting a name outside it stops the firing. The trigger needs no declaration, so a script can watch runs it never starts.

`workflow_finished` takes these options:

| Option | Default | Rule |
|--------|---------|------|
| `workflows` | every workflow | Workflow names, matched against the event's `workflow`, never against a run's display name |
| `statuses` | `["completed", "failed", "cancelled", "interrupted"]` | The terminal statuses that fire. At least one. |

Both lists hold at most 16 entries, without repeats.

- It fires for every run in this session that reaches a terminal status, whoever started it: the model's `workflow` tool, `/workflow`, or any automation.
- A run that is resumed and finishes again fires again, under the same `run_id`. Section 8 shows how to correlate such runs.
- A run that finishes while the session is closed, or that a shutdown interrupts, does not fire.
- A paused run, or one stopped at its agent budget, has not finished. It fires once it reaches a terminal status.
- Every completion also starts a model turn, as it does without automations, so the model sees the result whatever the script does with it.

### Limits

| Key | Default | Rule |
|-----|---------|------|
| `cooldown` | `"0s"` | At most `"24h"`. The shortest gap between two firings that act. |
| `max_per_hour` | `12` | 1 to 600. Firings that act, per rolling hour. |

A firing acts when it calls a host function other than `now`, `log`, `skip`, `release`, `pause_automations`, and the parsers. Section 9 explains what happens when a limit refuses.

### Args

Args let one script serve many sessions with a goal list, a file name, or a test command. The script declares them, and the user gives the values when arming.

```rhai
args: #{
    goals: #{ type: "list", min: 1, description: "Goal conditions, in order" },
    continuation_limit: #{ type: "int", default_value: 24, min: 1, max: 100 },
},
```

| Spec key | Meaning |
|----------|---------|
| `type` | `"string"`, `"int"`, `"float"`, `"bool"`, or `"list"` (a list of strings) |
| `default_value` | A literal of that type. Without one, the arg is required. |
| `min`, `max` | Bounds on a number, or on the length of a list |
| `choices` | The strings a `string` arg may take |
| `description` | At most 160 bytes. Shown when arming and in `list`. |
| `example` | The value `validate` uses for a required arg. Without one, `validate` derives a value from the type. |

- At most 16 args, with snake_case names.
- A string holds at most 4 KiB and is trimmed, so a blank string counts as missing. A list holds at most 64 strings. All values together take at most 16 KiB of JSON.
- Arming with an undeclared name is an error, and `validate` flags a body that reads an undeclared `args.x`.
- Args are trusted, so `message()` and `set_goal()` accept text built from them.
- Args cannot change the header. Triggers, schedules, limits, and capabilities stay as written, so `send(args.to, …)` must still match `meta.messaging.send`.

## 5. Event reference

Every event has these fields:

| Field | Meaning |
|-------|---------|
| `trigger` | The trigger kind, such as `"idle"` |
| `fire_id` | This firing's id |
| `at` | When the event happened, in unix seconds |
| `session` | `#{ id, title, name, mode, status, status_since, goal, cost, groups, work }` |

In `session`:

- `title` is untrusted.
- `name` is the session's messaging name, such as `"@builder"`, which other sessions use to reach it, or `()` when cross-session messaging is off.
- `mode` is the session's mode, such as `"build"`.
- `status` is `"working"`, `"needs_input"`, or `"idle"`, as other sessions see it: a message held for review counts as `"needs_input"`, and a ready plan does not. `status_since` is when it began, in unix seconds.
- `goal` is `#{ condition, evaluations }` while a goal is active, and `()` otherwise.
- `cost` is the session's spend in USD, or `()` when it is unknown.
- `groups` lists the consumer groups the session takes work from.
- `work` is `#{ held, paused }`. `held` is the work item the session works on, as `#{ group, work, attempt, max_attempts }`, or `()`. `paused` lists the items it paused that still wait for an outcome, as `#{ group, work, pause_reason }`.

A paused work item carries a `pause_reason`, here and in `idle.work` and `work_finished`. It waits for a person to retry or cancel it, and until then the agent that paused it can still report its outcome.

| `pause_reason` | The item paused because |
|----------------|-------------------------|
| `"completion_required"` | the turn working on it ended without reporting an outcome |
| `"cancelled"` | a person cancelled the turn working on it |
| `"turn_limit"` | the turn working on it reached its turn limit |
| `"turn_failed"` | the turn working on it failed |
| `"session_closed"` | the session closed while working on it |
| `"manual"` | a person paused it with `/groups pause` or `caudra message work pause` |

The fields of each trigger follow. Untrusted ones are marked U.

`armed`:

- `reason`: `"launch"` (armed as the session started, or by switching to a profile that lists it), `"resume"` (armed again with its stored args as the session resumed), `"manual"` (the user armed it, or armed it again), or `"unpaused"` (the pause latch cleared)

`idle`, about the busy period that just ended:

- `outcome`: `"completed"`, `"error"`, `"cancelled"`, or `"max_turns"`
- `error_kind`: `"rate_limit"`, `"overloaded"`, `"auth"`, `"timeout"`, `"network"`, `"other"`, or `()`
- `error` U: the error message, or `()`
- `last_response` U: the final response, at most 32 KiB
- `started_by`: `#{ kind }` for the first run of the period. `kind` is `"user"`, `"automation"` (with `automation` and `fire_id`), `"peer"` (a message, with `message_id`, `sender`, `sender_kind`, `audience`, and `topic`), `"work"` (a work item, with `group`, `work`, `attempt`, `max_attempts`, `message_id`, and `topic`), `"background"`, `"workflow"`, `"goal"`, or `"mailbox"`. For `"peer"` and `"work"`, `message_id` is the id the model saw the message by, and a script's message has `sender: ()`.
- `automations`: the names of the automations whose messages were delivered during the period
- `runs`: how many runs it held
- `busy_s`: its length in seconds
- `cost`: its spend in USD, or `()`
- `work`: the consumer-group work outcomes of the period, as `#{ group, work, outcome, pause_reason, detail }`. `outcome` is `"completed"`, `"retry"`, `"failed"`, or `"paused"`. `pause_reason` is set for `"paused"` and `()` otherwise. `detail`, the agent's summary or reason, is untrusted, or `()`.

`needs_input`:

- `input`: the kind of prompt, one of `inputs`
- `tool`: the tool a `permission` prompt is for, or `()`
- `waiting_s`: how long the session has waited

`goal_finished`:

- `verdict`: `"met"`, `"impossible"`, or `"cleared"`
- `condition`: the goal's condition
- `reason` U: the evaluator's reason, or for `cleared`, the error
- `evaluations`, `duration_s`, and `cost` (USD, or `()`)

`message_received`:

- `message_id`: the message's key. Pass it to `send()` as `reply_to` to answer this message; `reply()` cites it for you. It is not the id the model sees.
- `audience`: `"direct"`, `"topic"`, or `"broadcast"`
- `topic`: the topic of a topic message, or `()`
- `sender_kind`: `"session"` (another session's agent), `"automation"` (another session's automation), or `"script"` (a script that sent with `caudra message`)
- `sender`: the sending session's `@name`, or `()` for a script
- `sender_automation`: the name of the automation that sent it, or `()`
- `sender_label` U: a script's label, or `()`
- `sender_title` U: the sending session's title, or `()`
- `sender_cwd` U: the sender's working directory, or `()`
- `text` U: the message
- `reply_to`: the id of the message it answers, or `()`. When it answers a message this session sent, it is the `message_id` that `send()` or `reply()` returned.
- `admission`: `"queued"` or `"held"`, as for the `admissions` option
- `delivery`: `"live"` as it was sent, or `"catch_up"` for an earlier topic message the session caught up on
- `consumed`: `true` when this automation took the message from the model

`work_finished`, about an item of work this session published:

- `group` and `work`: the item's group and name
- `message_id` and `topic`: the publication that queued it. `message_id` is the one `publish()` or `broadcast()` returned.
- `state`: `"completed"`, `"failed"`, `"cancelled"`, or `"paused"`
- `attempts` and `max_attempts`: the attempts it used, and the most it gets
- `member`: the `@name` of the member that held it last, or `()` when no member with a name holds it, as for an item cancelled before anyone took it or returned to the queue for a retry
- `pause_reason`: for `"paused"`, and `()` otherwise
- `detail` U: the agent's summary or reason, or `()`

`workflow_finished`, about a workflow run in this session:

- `run_id`: the run's id, which `start_workflow()` returns. A resumed run keeps it.
- `name`: the run's display name, unique within the session, which `start_workflow()` also returns
- `workflow`: the workflow's name, which the `workflows` option filters on
- `status`: `"completed"`, `"failed"`, `"cancelled"`, or `"interrupted"`
- `report` U: the run's report, at most 256 KiB and cut with `…[truncated]` at the end, or `()`
- `result` U: the run's result as an untrusted structure without its `report` key, or untrusted text cut the same way when it is larger than 256 KiB, or `()`
- `error` U: the run's error, or `()`
- `scratch_dir`: the run's scratch directory, or `()`
- `agents` and `tokens`: how many agents the run used, and how many tokens they spent

`schedule`:

- `scheduled_for`: the occurrence, in unix seconds
- `late_by_s`: how late the firing is

Every field not marked U is trusted. Goal conditions are trusted because `set_goal()` refuses untrusted text. Group and work names are trusted too, because people name groups and Caudra names work items, and so are workflow names and run names.

## 6. Host API

These functions are a firing's only way to reach the session.

### `message(text)` and `message(text, options)`

Queues `text` for this session's model. `text` must be trusted. Returns `()`.

| Option | Meaning |
|--------|---------|
| `delivery` | `"next"` (default) starts a turn once the session settles. `"guide"` joins a running turn before its next model request, like queued guidance, and starts a turn when the session is idle. |
| `attach` | Any value, shown to the model after the text as a framed, untrusted JSON block. This is the only way to show untrusted values to the model. |
| `expires` | A duration of at most `"7days"`. The message is dropped if it has not been delivered by then. |

The model receives the text under a header that names the automation. Messages wait in an outbox until the session can take them, so the session limits in section 9 can delay them.

### `set_goal(condition)` and `set_goal(condition, options)`

Sets the session goal and queues its kickoff turn like a `next` message. `condition` must be trusted. Returns `#{ condition }` with the condition as the goal stored it. Keep that value to compare with `event.condition` when the goal finishes.

| Option | Meaning |
|--------|---------|
| `continuation_limit` | A positive integer: the goal's automatic-continuation limit, as `/goal` shows it |
| `replace` | `true` replaces an active goal. Without it, `set_goal` fails with kind `goal_active` while another goal is active. |
| `expires` | As for `message` |

### `skip(reason)`

Ends the firing without acting and records `reason`, which is required. A skip commits the firing's `state` changes, as completing does. `try` does not catch a skip.

### `notify(text)`

Shows `text` to the user as a notification, subject to their notification settings. Untrusted text is accepted and sanitized for the terminal.

### `http(request)`

Sends one HTTP request and returns `#{ status, body, json }`. `request` is a map:

| Key | Meaning |
|-----|---------|
| `method` | Required: `"GET"`, `"POST"`, `"PUT"`, `"PATCH"`, or `"DELETE"` |
| `url` | The URL. Its origin must be in `meta.network`. |
| `url_env` | Instead of `url`, a variable in `meta.secrets` that holds the URL, for a webhook whose URL is its secret. The host reads it and checks its origin against `meta.network`. |
| `query` | A map of text values, percent-encoded and added to the URL |
| `headers` | A map of header names to text values. Requests carry Caudra's `User-Agent` unless `headers` or `secret_headers` sets one. |
| `bearer_env` | A variable in `meta.secrets` whose value is sent as `Authorization: Bearer <value>` |
| `secret_headers` | A map of header names to variables in `meta.secrets` whose values are sent |
| `json` | Any value, sent as JSON with `Content-Type: application/json` unless `headers` sets a content type |
| `body` | Text, sent as it is. Give `json` or `body`, not both. |
| `timeout` | A duration of at most `"30s"`, the default. What is left of the firing's 120 seconds of wall time caps it. |

Secrets stay with the host. The script names a variable and never sees its value, and history shows the variable names and only the origin of a `url_env` target. If a response repeats a secret, `body` and `json` show `${NAME}` in its place, where `NAME` is the variable, or the origin in place of a `url_env` URL. Caudra reads the variables from its own environment and the global `.env` file. A project's `.env` cannot set them, so tell the user which variables to set.

Every status comes back as an answer, 404 and 500 included, so test `status` before you trust the body. `body` is the response text, cut to 1 MiB. `json` is the body parsed as JSON, or `()` when it does not parse or was cut. Both are untrusted. Caudra sends each request once and never retries it.

Caudra follows a redirect only within the request's origin. A GET without a body follows any redirect there. Any other request follows only a 307 or 308 there, sent again with the same method and body. Any other redirect, including one to another origin, comes back as the answer with its 3xx status. Response headers are not shown, so request the final URL directly and declare its origin.

A request fails only when no response arrives, or when the host cannot send it. `try` catches these failures:

| Kind | Cause |
|------|-------|
| `transport` | Connecting, sending, or reading the response failed |
| `timeout` | No response arrived within `timeout` or the firing's remaining wall time |
| `refused` | A variable in `meta.secrets` is unset or empty, the URL carries a user name or password, or the network policy refuses the target, such as a host name that resolved to a loopback or private address |
| `invalid_argument` | A bad option, an invalid header name or value, a header given twice, `Authorization` beside `bearer_env`, a header the HTTP client sets itself (`Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Keep-Alive`, `TE`, `Trailer`, `Upgrade`, `Expect`, or any `Proxy-*` header), or a body over 1 MiB |

These stop the firing before anything is sent, even when the request also holds a failure that `try` would catch: an origin outside `meta.network`, including the origin of a `url_env` value, a variable outside `meta.secrets`, and a `url_env` value that is not an http or https URL.

Loopback and private hosts, such as `localhost` and `192.168.1.10`, are out of reach unless the user sets `[automations] allow_private_network = true` in the global config. A project config cannot set it. Without it, a loopback or private target stops the firing, and a host name that resolves to such an address fails with `refused`.

The `goal-webhook` example in section 12 posts goal outcomes with a bearer token, and `ci-watch` polls an API and publishes what it finds.

### `send(to, text)` and `send(to, text, options)`

Sends `text` to another session as a direct message. `to` is an `@name` that `meta.messaging.send` allows, written with its `@`, such as `event.sender`. The one option, `reply_to`, cites the message this one answers by its id: `send(event.sender, "Done", #{ reply_to: event.message_id })`.

Returns `#{ status, message_id, reason }`, where `message_id` is the new message's id:

| `status` | Meaning |
|----------|---------|
| `"queued"` | The recipient admitted it for its model |
| `"held"` | It waits for the recipient's review, or for the recipient to resume, as `reason` says |
| `"unknown"` | The connection failed after sending, so the recipient may have it. Do not send it again. |

### `reply(text)`

Answers the sender of the message the firing consumed, citing it as `reply_to`, and returns a receipt as `send` does. It needs `reply: true` in `meta.messaging` and an event with `consumed: true`. A script has no reply target, so a reply to a script's message fails with `no_reply_target`. Replying does not end the firing, and a firing that replies and completes keeps the message from the model.

### `release(reason)`

Ends the firing and hands the consumed message back to normal delivery, so the model receives it. It needs an event with `consumed: true`. As with `skip`, `reason` is required and recorded, the firing keeps its `state` changes, and `try` does not catch the call. Releasing costs nothing against the automation limits.

### `publish(topic, text)` and `broadcast(text)`

`publish` sends `text` to the sessions subscribed to `topic`, a concrete topic listed in `meta.messaging.publish`. `broadcast` sends it to every session that receives broadcasts, and needs `"broadcast"` in that list. A publication on a consumer group's topic also queues one work item in that group.

A publication succeeds once the message history records it, whatever its recipients do with it. Both return `#{ message_id, audience, recipients, skipped, queued }`:

- `recipients`: `#{ name, title, status, reason }` for each live session it went to. `status` is `"queued"`, `"held"`, `"refused"`, `"unavailable"`, `"rate_limited"`, or `"unknown"`, and `title` is untrusted.
- `skipped`: how many matching sessions the fan-out limit left out
- `queued`: `#{ group, work }` for each work item it queued. Queued is not done: `work_finished` reports the outcome.

```rhai
let receipt = broadcast("Deploy freeze until 18:00. Do not push to main.");
let missed = receipt.recipients.filter(|r| !(r.status in ["queued", "held"]));
if !missed.is_empty() { notify("The freeze notice missed " + missed.len() + " sessions"); }
```

### Sending as the session

A message an automation sends goes out as its session, marked as the automation's. Recipients see `sender_kind: "automation"` and the automation's name, and their `auto` policy judges the message as it judges the session's own. Unlike the session's agent, an automation can send while the session waits on a person, or after a cancelled or failed run has stopped its automatic wakes. Everything else applies as to any message: a ReadOnly session cannot send, the pause latch stops the firing, and the recipient's inbound policy, the messaging rate limits, and the group limits hold. The message history records each one.

A messaging call fails, and `try` catches it, when the message cannot go out or its recipient does not admit it:

| Kind | Cause |
|------|-------|
| `refused`, `rate_limited`, `unavailable` | The recipient of `send()` or `reply()` did not admit the message. The kind is its delivery status, and the error's `message` says why. `unavailable` also means the session is not connected to the other sessions at the moment. |
| `unknown_recipient` | No live session holds the `@name` |
| `group_full` | A publication would exceed the unfinished work a consumer group, or all groups together, may hold, or the fan-out limit |
| `read_only` | The session is in ReadOnly mode |
| `no_reply_target` | `reply()` to a message a script sent |
| `invalid_argument` | The text is empty or longer than 32 KiB, or `reply_to` is not a `message_id` from a `message_received` event |

A publication also fails with `rate_limited` when the session has reached its publication rate, and with `unavailable` when the message history cannot record it. Either way no session receives it.

### `start_workflow(name, args)` and `start_workflow(name, args, options)`

Starts a run of the workflow `name` in this session, in the background, and returns `#{ run_id, name }`, where `name` is the run's display name. `name` must be in `meta.workflows`. `args` is the map of args the workflow reads, passed as it is: a start never checks it, so a missing or mistyped arg fails the run, not the call. The one option, `agent_budget`, is a positive integer: the agents the run may admit in total, as the `workflow` tool takes it.

```rhai
let run = start_workflow("review-changes", #{ scope: "the last commit" }, #{ agent_budget: 12 });
state.run = run.run_id;
```

- The call counts as an action under the automation limits, like `http()`.
- The run belongs to the session. It runs on even when the firing is stopped or fails afterwards. That firing's `state` changes are discarded, so a run started before a failure goes unrecorded. Catch the failures that may follow a start, as `research-desk` in section 12 does.
- A session holds at most 4 active runs, and the model's `workflow` tool shares that cap.
- `workflow_finished` reports the outcome. Keep `run_id` to recognise it.

A start fails, and `try` catches it, when the run cannot begin:

| Kind | Cause |
|------|-------|
| `invalid_argument` | A bad `agent_budget`: one that is not a positive integer, or one over the most a run may admit |
| `refused` | The workflow is not trusted, unknown, ambiguous, or invalid. 4 runs are already active. Background admission is closed, for example after Esc Esc until the user's next turn. A script never reopens it. |
| `unavailable` | The session has no workflow runtime, or a storage or internal error |

### `log(text)`

Adds a line to the firing's trace, which `history` and `/automations` show. Untrusted text is accepted. `log` takes text only, so write `log("" + count)` for a number. A firing may log at most 64 lines.

### `now()`

Returns `#{ unix, iso, date, weekday, hour, minute, tz }` in `meta.timezone`, or in the system zone without one. `date` looks like `"2026-10-05"`, `weekday` is `"mon"` to `"sun"`, and `hour` and `minute` are integers.

### `pause_automations(reason)`

Sets the session's pause latch, as Esc Esc does. Every automation in the session stops: running firings are cancelled, and later events are recorded as `paused`. The calling firing runs on to its end, so it can still notify the user. The automation limits never refuse this call.

### Parsers

`parse_json`, `one_of`, `parse_int`, and `parse_float` turn values into data a script can test. Section 7 covers them.

### Per-firing limits

A firing may use at most 1 million operations, 120 seconds of wall time including host calls, 32 actions, 64 log lines, and 4 calls to `message` and `set_goal` together. Reaching any of them stops the firing.

### Errors

Two kinds of error end a firing:

| Kind | Examples | Effect |
|------|----------|--------|
| Stop | untrusted text in `message()` or `set_goal()`, the `${}` placeholder in any host function, a target, topic, or `reply()` that `meta.messaging` does not allow, a workflow outside `meta.workflows`, `reply()` or `release()` without a consumed message, a per-firing limit, an automation limit at the first action, a pause, a disarm | The firing ends at once and its `state` changes are discarded. `try`/`catch` cannot intercept it. |
| Failure | `goal_active`, a messaging failure such as `unknown_recipient`, a `start_workflow()` failure such as `refused`, an invalid argument such as `delivery: "now"`, `throw`, and any ordinary Rhai error | `try { … } catch (err) { … }` catches it. Uncaught, the firing fails and its `state` changes are discarded. |

In `catch (err)`, a host failure binds `#{ kind, message }`, where `kind` is a name such as `"goal_active"` or `"invalid_argument"`. An ordinary Rhai error binds Rhai's own map, which has `message` and `line` but no `kind`, so `err.kind` reads as `()`. `throw value` binds `value` itself.

```rhai
try {
    set_goal(args.condition);
} catch (err) {
    if err.kind == "goal_active" { skip("another goal is active"); }
    throw err;
}
```

Every stop and failure is recorded with its kind and message, and with its line and column when Rhai knows them. Effects that happened before it, such as a queued message or a started run, are not undone.

## 7. Untrusted values

Text from outside the script arrives as an untrusted value: the session title, the model's last response, error messages, goal reasons, messages from other sessions, workflow reports and results, and everything `parse_json` returns. `type_of(value)` is `"untrusted"`. An untrusted value holds text or a JSON structure.

| Operation | Result |
|-----------|--------|
| `+` with an untrusted operand | Untrusted. Taint is contagious. |
| `to_lower`, `to_upper`, `sub_string`, `split`, and `trim` or `replace` in place | Untrusted |
| `==`, `!=`, `in`, `contains`, `starts_with`, `ends_with`, `index_of`, `matches(regex)`, `len`, `is_empty` | A plain bool or integer |
| `one_of(value, ["a", "b"])` | The matching string from the script's own list, as plain text, or `()` |
| `parse_int(value)`, `parse_float(value)` | A plain number, or `()` when it does not parse |
| `parse_json(value)` | An untrusted structure, or `()` when it does not parse. Reading a key or an index gives plain numbers, bools, and `()`, or untrusted text and structures. `keys()` gives an array of untrusted text, and `for` walks the items of an array or the keys of an object. |
| `${value}` and `value.to_string()` | A placeholder that every host function refuses |
| `text += value` with plain `text` | An error. Write `text = text + value`. |
| `for c in value` with untrusted text | An error |

The text of `message()` and the condition of `set_goal()` must be trusted, because they become this session's instructions. Every other host function accepts untrusted values, `reply()`, `send()`, and `start_workflow()` included, and so does `state`.

Act on what a test of the value says:

```rhai
if event.last_response.contains("BLOCKED") {
    message("You reported a blocker. Describe it in one sentence, then try another approach.");
}
```

Show the value itself to the model with `attach`:

```rhai
message("The last turn failed with the attached error. Fix the cause and continue.", #{ attach: event.error });
```

Turn a field into one of your own strings with `one_of`:

```rhai
let status = one_of(parse_json(event.last_response)?.status, ["done", "blocked"]);
if status == "blocked" { message("Explain the blocker in one sentence, then try another approach."); }
```

The marking keeps outside text from steering the session, and `attach` is the supported way to show it. Do not rebuild untrusted text from predicates to slip it into `message()`.

**Workflow args lose the mark.** `start_workflow()` accepts untrusted values in `args`, and they reach the workflow as plain data, because workflow args are data. The workflow then decides what its agents see. Pass text from another session or from `http()` only to a workflow that treats its args as data and frames them for its agents as material to work on. Never pass it to a workflow that pastes an arg into its agents' instructions, because the text would then steer those agents. The `${}` placeholder is refused here as everywhere.

## 8. State and args

`state` is the automation's memory in one session. It is a map, empty until a firing commits something.

- **Commit rule:** a firing that completes commits its `state` changes. It completes when it runs to the end, returns, or calls `skip` or `release`. A firing that fails, is stopped, or is cancelled commits nothing. Effects it already caused are not undone.
- **Removal:** setting a key to `()` removes it at commit.
- **Size:** at most 64 KiB serialized. A firing that ends over the limit fails.
- **Shape:** `state` stays a map. Assigning anything else to it fails the firing.
- **Taint:** untrusted values may be stored, and they stay untrusted.
- **One at a time:** one firing per automation runs at a time, so firings never race on `state`.
- **Lifetime:** `state` survives disarming, new args, and script edits. A fork starts empty, and resume restores it. The user can edit or clear it in `/automations`. A firing that was running at the time keeps its effects and commits nothing.

Patterns that follow from the commit rule:

- **Record and act in the same firing.** If the action fails, the record goes with it, and the next firing tries again.
- **Catch what you can afford.** Wrap an action in `try`/`catch` when its failure should not cost the record.
- **Correlate by value.** Keep what identifies the work you started, such as the condition `set_goal` returned, a `message_id`, or a work item's name, and compare it with the event. A counter that assumes every firing succeeded drifts.
- **Map runs to requests.** To answer for each run, keep a map from `run_id` to what the run is for, as `research-desk` does, and look the run up when `workflow_finished` fires. A run that is resumed and finishes again fires again under the same `run_id`. Remove its entry only on the status you treat as final, such as `"completed"`, or keep the entry and act on `event.status`. An entry removed on the first status, whatever it is, ignores the run's later finishes.

Args are stored with the session, so they come back on resume. A profile's args only seed the first arming, so later edits survive. `--automation NAME=…` replaces the stored args and fires `armed` with reason `launch`. Arming again with new args during the session fires reason `manual`. If a script edit makes the stored args invalid, the automation stays disarmed until the user fixes them in `/automations`.

## 9. Limits and loops

Automations start turns while nobody watches, so several limits apply.

**Automation limits.** The first action of a firing checks `cooldown`, `max_per_hour`, and the failure backoff. A firing that only reads, logs, skips, or releases costs nothing.

- When a limit refuses, the firing stops before acting and its `state` changes are discarded.
- A one-shot event, `armed`, `goal_finished`, `message_received`, `work_finished`, or `workflow_finished`, is deferred and retried once the limit allows. Later events wait behind it, and a consumed message waits with its event.
- A recurring event, `idle`, `needs_input`, or `schedule`, is recorded as `rate_limited` and not retried.
- Events wait in a queue of 16 per automation. A newer `armed`, `idle`, `needs_input`, or `schedule` event replaces a waiting one of its kind, while each message, work, and workflow event keeps its place. When the queue is full, the oldest event is dropped.
- After a failed firing, the next acting firing waits 1 minute, doubling up to 30 minutes. A firing that acts and completes, or arming again, resets this backoff. A firing that completes without acting leaves it in place. The failed event is not retried.

**Session limits.** These cover every automation in the session together. The user sets them in the global config.

- `[automations] turns_per_hour` (default 20) caps the turns automations start per rolling hour. Messages wait in the outbox until there is room.
- `[automations] max_unattended_turns` (unset by default) stops automation-started turns after that many since the last human input.
- When automation-started turns keep ending in error, the next delivery waits 1 minute, doubling up to 30 minutes. A clean run or human input resets this backoff.

**Pause latch.** Esc Esc or `pause_automations()` pauses every automation in the session. Running firings are cancelled, and later events are recorded as `paused` without running. The next human input clears the latch and fires `armed` with reason `unpaused`, so a script that handles `armed` can pick up where it stopped. Esc Esc also closes background admission until the user's next turn, and `start_workflow()` fails with `refused` while it is closed. No script can reopen it.

**Loops.** An `idle` firing that calls `message` starts a turn, and that turn ends in another `idle`. Give every such loop a way out:

- A stop condition in the body, such as a marker the model writes when the work is done.
- A check of `event.outcome`, so an error or a cancelled turn does not start another.
- `max_per_hour` and `cooldown` sized to the work.
- `event.started_by` and `event.automations`, to tell the turns this automation started from the turns a person started.

**Loops between sessions.** A message from an automation can fire an automation in the session it reaches, and the answer can fire the first one again. `message_received` ignores what other sessions' automations send unless the trigger sets `from_automations: true`, so by default two automations cannot keep each other going. Give such a trigger a way out too, such as answering only messages that answer nothing (`event.reply_to == ()`). An automation and another session's model can still keep a conversation going. `max_per_hour` and the messaging rate limits slow it, and Esc Esc in the automation's session stops it.

**Loops through workflows.** A script that starts a workflow from `workflow_finished` starts a run whose end fires it again, and a run started from `idle` ends in a model turn that ends in another `idle`. Bound such a loop with `max_per_hour` and with state, such as a count of the runs started for one request or a stop on `event.status`. A session holds at most 4 active runs, and the model's runs count toward the cap too, so a script that keeps the cap full leaves the model unable to start one. A start over the cap fails with `refused`.

**Work runs at least once.** A work item whose lease ran out returns to its group's queue, so another member may repeat it after the first had effects, and `/groups retry` runs an item again. `work_finished` and `idle.work` can therefore report the same item more than once. Keep the work a script publishes safe to repeat, and key what the script records on the work name, as `work-nudge` in section 12 does.

## 10. Rhai for automation authors

Rhai looks like a small JavaScript with Rust flavour. These are the points that bite in automation scripts. Each was checked against the engine Caudra ships.

- **Values:** `1` (64-bit integer), `1.5` (float), `"text"`, `true`, `[1, 2]`, `#{ key: "value" }`, and `()`, the unit value that means nothing. `type_of(x)` names the type.
- **Missing keys:** reading a missing key of a map gives `()` without an error. A misspelt field such as `event.last_reponse` reads as `()` and fails later with `Function not found`. Reading a key of `()` is an error, so `event.session.goal.condition` fails when there is no goal. Write `event.session.goal?.condition ?? "none"`.
- **Defaults:** `x ?? fallback` gives `fallback` when `x` is `()`. `state.count += 1` fails while `count` is missing. Write `state.count = (state.count ?? 0) + 1`.
- **Conditions:** `if` takes only a bool. Compare with `()` explicitly: `if event.session.goal != () { … }`.
- **Membership:** `x in list`, `"key" in map`, and `"part" in text` give a bool.
- **switch:** `switch event.trigger { "armed" => …, "goal_finished" => …, _ => … }`.
- **Closures:** `args.goals.find(|goal| !(goal in done))` returns the first match or `()`. A closure can read the variables around it. `filter`, `map`, `some`, `all`, and `reduce` take closures too.
- **Functions:** `fn name(a, b) { … }` cannot see `event`, `state`, `args`, or any other outer variable. Pass in what it needs and return the result.
- **Numbers:** `7 / 2` is `3` and `7.0 / 2` is `3.5`. Integer overflow is an error. An integer compares with a float as expected. `parse_int("abc")` on plain text is an error, while on an untrusted value it gives `()`.
- **Strings:** `+` joins a string with any value, so `"n=" + 3` is `"n=3"` and `"a" + () + "b"` is `"ab"`. `trim` and `replace` change the string in place and return `()`, so `let t = s.trim();` leaves `t` as `()`. Call `s.trim();` and then use `s`.
- **Interpolation:** `` `${minutes} minutes` `` works for trusted values. An untrusted value turns into a placeholder that every host function refuses, so join it with `+` instead.
- **Keyword keys:** `#{ default: 1 }`, `#{ for: 1 }`, and `m.default` fail to parse. Quote the key: `#{ "default": 1 }` and `m["default"]`.
- **Output:** `print` and `debug` are disabled. Use `log(text)`.
- **Waiting:** `sleep` and `exit` are unavailable. End a firing with `return` or `skip(reason)`, and wait with `after`, `cooldown`, or a `schedule` trigger.
- **Errors:** `try { … } catch (err) { … }` catches failures, `throw`, and runtime errors such as an index out of range. It never catches a stop, a skip, or an automation limit. `try` is a statement, so assign to a variable declared before it.
- **Operations:** every operation counts toward the limit, so a runaway loop stops the firing instead of hanging it.

## 11. Write, validate, and hand over

1. Call `automation` with `action: "list"`. Note the two directories and check that the name is free.
2. Write `<dir>/<name>.rhai` with `file_write`. The header comes first.
3. Validate with `automation`, `action: "validate"`, and `name: "<name>"`. Validation parses the header, compiles the script, checks the args the body reads, and runs the body once per trigger against a canned event. Each run gets every arg's default or `example`, an empty `state`, and a session without a goal or a cost, and it performs nothing. A `message_received` run gets a message its trigger matches, consumed when the trigger consumes. A `workflow_finished` run gets a run of the first workflow its trigger names, or else the first in `meta.workflows`, with the first of its `statuses`. A stop or an uncaught failure fails the validation with its line and column. Each run follows one path, so branches that depend on real event values are not reached. Fix the file and validate again.
4. Hand over. Trusting a project script, arming it, and choosing its args belong to the user. You can do none of them, so end your turn with what the user must do. A project script needs their trust in `/automations` first. Then give the exact line that arms it, with the args filled in:

   ```
   /automations arm goal-chain {"goals": ["The login tests pass", "The signup tests pass"]}
   ```

   To arm it at launch instead:

   ```
   caudra --automation goal-chain='{"goals": ["The login tests pass", "The signup tests pass"]}'
   ```

   `--automation NAME=@goals.json` reads the args from a file, and a bare `--automation NAME` arms with the defaults. Repeat the flag to arm more automations, naming each once.

   A messaging script also needs cross-session messaging, which only the user can turn on, and a session with the name, topics, and groups its triggers expect. Give that launch line too, such as `caudra --name worker-1 --group swarm-tasks --automation status-beacon`.

   A workflow script also needs `experimental.workflows`, which only the user can turn on. Every workflow in `meta.workflows` must exist, and the user must approve a project workflow in `/workflows`. Name those workflows in the answer.
5. Debug with `automation` and `action: "history"`. It lists this session's newest firings with their status, only one automation's when you pass `name`. Pass `fire_id` to see one firing's event and its actions, log lines included. A skipped firing shows its reason, and a failed one shows its error and line. When you fix a script that has already fired, add to the hand-over that the user can press `r` on one of its finished firings in `/automations` to replay that event against the edited file, a dry run that performs nothing.

## 12. Complete examples

Each example below validates, and the test suite replays it against scripted events. The last six, from `ci-watch` on, are for sessions with cross-session messaging turned on. `nightly-review` needs workflows turned on, and `research-desk`, the last, needs both messaging and workflows.

### keep-going

Works through a backlog file during work hours.

```rhai
let meta = #{
    name: "keep-going",
    description: "Work through a backlog file during work hours",
    triggers: [#{ kind: "idle", after: "2m" }],
    limits: #{ max_per_hour: 6 },
    args: #{
        file: #{ type: "string", default_value: "TODO.md", description: "Backlog with checkboxes" },
        from_hour: #{ type: "int", default_value: 9, min: 0, max: 23 },
        until_hour: #{ type: "int", default_value: 18, min: 1, max: 24 },
    },
};
let t = now();
if (t.weekday in ["sat", "sun"]) || t.hour < args.from_hour || t.hour >= args.until_hour { skip("outside work hours"); }
if event.outcome != "completed" { skip("the last turn ended " + event.outcome); }
if event.last_response.contains("BACKLOG EMPTY") { skip("the backlog is empty"); }
message("Continue with the next unchecked item in " + args.file + ". When none remain, reply with BACKLOG EMPTY.");
```

The `idle` trigger waits for two quiet minutes. The script skips outside work hours, after a turn that did not complete, and once the model reports the marker. `max_per_hour: 6` bounds the loop. Arm it with `/automations arm keep-going {"file": "TODO.md"}`.

### goal-chain

Pursues a list of goals in order.

```rhai
let meta = #{
    name: "goal-chain",
    description: "Pursue a list of goals in order, starting with the first when armed",
    triggers: [#{ kind: "armed" }, #{ kind: "goal_finished" }],
    args: #{
        goals: #{ type: "list", min: 1, description: "Goal conditions, in order" },
        continuation_limit: #{ type: "int", default_value: 24, min: 1, max: 100 },
    },
};
let done = state.done ?? [];
if event.trigger == "goal_finished" && event.condition == state.current {
    state.current = ();
    if event.verdict != "met" {
        notify("goal-chain stopped: verdict " + event.verdict);
        return;
    }
    done.push(event.condition);
    state.done = done;
} else if event.session.goal != () {
    skip("another goal is active");
}
let next = args.goals.find(|goal| !(goal in done));
if next == () {
    notify("goal-chain: all " + args.goals.len() + " goals are met");
    return;
}
state.current = set_goal(next, #{ continuation_limit: args.continuation_limit }).condition;
```

Armed with `{"goals": ["A", "B", "C"]}`, it runs like this:

| What happens | Event | Script | `state` afterwards |
|--------------|-------|--------|--------------------|
| The session starts without a goal | `armed` (`launch`) | Sets A | `{current: A}` |
| A is met | `goal_finished` (`met`, A) | Records A and sets B | `{done: [A], current: B}` |
| Caudra exits during B and resumes | `armed` (`resume`), with B restored first | Skips, because a goal is active | unchanged |
| B is judged impossible | `goal_finished` (`impossible`, B) | Notifies and stops | `{done: [A]}` |
| The user fixes the blocker and arms it again | `armed` (`manual`) | Sets B again | `{done: [A], current: B}` |
| `set_goal` hits `max_per_hour` | any | Stops before acting. The event is deferred and retried. | unchanged |
| C is met | `goal_finished` (`met`, C) | Notifies that every goal is met | `{done: [A, B, C]}` |

Comparing `event.condition` with `state.current` leaves alone any goal the chain did not set. While such a goal is active, `armed` skips, and when it finishes, the chain starts its next goal.

### timebox

Asks a turn that has worked for an hour to wrap up.

```rhai
let meta = #{
    name: "timebox",
    description: "Ask a turn that has worked for an hour to wrap up",
    triggers: [#{ kind: "schedule", every: "5m" }],
    limits: #{ cooldown: "30m" },
};
let minutes = (now().unix - event.session.status_since) / 60;
if event.session.status == "working" && minutes >= 60 {
    notify("Still working after " + minutes + " minutes");
    message("You have worked on this for an hour. Finish the current step, then summarize progress and what remains.", #{ delivery: "guide", expires: "10m" });
}
```

The schedule checks every five minutes, and integer division turns seconds into whole minutes. The `guide` delivery reaches the running turn before its next model request, and `expires` drops the message if it has not reached the model within ten minutes. `cooldown: "30m"` keeps the automation from asking again right away.

### spend-guard

Pauses automations once the session has spent $20.

```rhai
let meta = #{
    name: "spend-guard",
    description: "Pause automations once this session has spent $20",
    triggers: [#{ kind: "idle" }],
    arm: "always",
};
if (event.session.cost ?? 0.0) > 20.0 {
    pause_automations("spend-guard: this session passed $20");
    notify("Automations paused: this session has spent over $20.");
}
```

`arm: "always"` arms it in every session, so the file must live in the user directory. An unknown cost reads as `()`, which `?? 0.0` turns into a number. The automation limits never refuse `pause_automations`, and the firing runs on to its end, so it can still notify.

### retry-overload

Resumes after rate-limit and overload errors.

```rhai
let meta = #{
    name: "retry-overload",
    description: "Resume after rate-limit and overload errors",
    triggers: [#{ kind: "idle", after: "1m" }],
    limits: #{ max_per_hour: 4 },
};
if event.outcome == "error" && (event.error_kind in ["rate_limit", "overloaded"]) {
    message("The previous turn stopped on a provider error. Continue where you left off.");
}
```

`after: "1m"` gives the provider a minute before the retry, and `max_per_hour: 4` caps the retries. When automation-started turns keep failing, the session's delivery backoff spaces them out further.

### goal-webhook

Posts every goal outcome to a webhook.

```rhai
let meta = #{
    name: "goal-webhook",
    description: "Post goal outcomes and record blockers when a goal is impossible",
    triggers: [#{ kind: "goal_finished" }],
    network: ["https://hooks.example.com"],
    secrets: ["HOOK_TOKEN"],
    arm: "always",
};
let response = http(#{
    method: "POST",
    url: "https://hooks.example.com/caudra",
    bearer_env: "HOOK_TOKEN",
    json: #{ session: event.session.title, verdict: event.verdict, reason: event.reason },
});
if event.verdict == "impossible" && response.status == 200 {
    message("The goal was judged impossible. Record what blocked it in BLOCKERS.md.", #{ attach: event });
}
```

The header declares the webhook's origin in `network` and its token in `secrets`. The script names `HOOK_TOKEN` and never sees its value, so tell the user to set it in their environment or the global `.env` file. The untrusted title and reason travel as JSON, which `http()` accepts. The script asks the model to record a blocker only after the webhook answered 200. A timeout or a transport failure is not caught, so it fails the firing before the message. `arm: "always"` arms it in every session, so the file must live in the user directory.

### nightly-review

Reviews the day's commits at 02:00 and posts the report to Slack.

```rhai
let meta = #{
    name: "nightly-review",
    description: "Review the day's commits at 02:00 and post the report to Slack",
    triggers: [
        #{ kind: "schedule", at: "02:00", catch_up: "skip" },
        #{ kind: "workflow_finished", workflows: ["review-changes"] },
    ],
    workflows: ["review-changes"],
    network: ["https://hooks.slack.com"],
    secrets: ["SLACK_REVIEW_URL"],
};
if event.trigger == "schedule" {
    state.run = start_workflow("review-changes", #{ scope: "commits from the last 24 hours on main" }).run_id;
} else if event.run_id == state.run {
    http(#{ method: "POST", url_env: "SLACK_REVIEW_URL", json: #{ text: "Nightly review " + event.status + "\n\n" + event.report } });
}
```

The schedule starts a run of the `review-changes` workflow, declared in `meta.workflows`, and `state.run` keeps its `run_id`. `workflow_finished` fires for every `review-changes` run in the session, including the ones the model or the user starts, so the script posts only when the run is the one it started. `catch_up: "skip"` drops a review the closed session missed instead of running it late. The Slack webhook's URL is its secret, so `url_env` names the variable and `network` declares its origin. The untrusted report travels as JSON, which `http()` accepts. The script keeps `state.run` after the post, so a run resumed and finished again posts again with its new status. The completion also starts a model turn, as every workflow completion does. The script needs workflows turned on, and `SLACK_REVIEW_URL` set in the user's environment or the global `.env` file.

### ci-watch

Publishes each failed CI run on `main` to `ci.failures`.

```rhai
let meta = #{
    name: "ci-watch",
    description: "Poll GitHub Actions on main and publish new failures to ci.failures",
    triggers: [#{ kind: "schedule", every: "10m" }],
    network: ["https://api.github.com"],
    secrets: ["GITHUB_TOKEN"],
    messaging: #{ publish: ["ci.failures"] },
};
let response = http(#{
    method: "GET",
    url: "https://api.github.com/repos/acme/app/actions/runs",
    query: #{ branch: "main", per_page: "1" },
    bearer_env: "GITHUB_TOKEN",
    headers: #{ Accept: "application/vnd.github+json", "User-Agent": "caudra-ci-watch" },
});
if response.status != 200 { log("GitHub returned " + response.status); return; }
let run = response.json.workflow_runs[0];
if run == () || run.id == state.last_run { return; }
state.last_run = run.id;
if run.conclusion == "failure" {
    publish("ci.failures", "CI failed on main: " + run.display_title + " " + run.html_url);
}
```

`network` lets `http()` reach the GitHub API, and `bearer_env` sends the token named in `secrets` without the script seeing it. The host percent-encodes `query`. `http()` throws only for transport errors and timeouts, so the script checks the status itself and logs anything but 200. Numbers read from `response.json` are plain, so `run.id` compares and stores as it is, while the title and the URL stay untrusted text. An empty list of runs reads as `()`. `publish()` accepts the untrusted text, and every session subscribed to `ci.failures` receives it as a topic message. State commits only when a firing finishes cleanly, so a publication that throws leaves `state.last_run` as it was, and the next check tries again while that run is still the newest.

### status-desk

Answers status questions from other sessions without a model turn.

```rhai
let meta = #{
    name: "status-desk",
    description: "Answer status questions from other sessions without a model turn, and pass every other direct message to the model",
    triggers: [#{ kind: "message_received", audiences: ["direct"], consume: true }],
    limits: #{ max_per_hour: 30 },
    messaging: #{ reply: true },
};
if event.sender == () { release("a script cannot take a reply"); }
let question = event.text.to_lower();
question.trim();
if !(question in ["status", "status?"]) { release("not a status question"); }
let s = event.session;
let goal = s.goal?.condition ?? "none";
reply(s.name + " (" + s.title + ") is " + s.status + ". Goal: " + goal + ".");
```

The trigger consumes every direct message, so the model sees only what the script hands back with `release()`: a script's message, which has no reply target, and anything that is not a status question. `to_lower` keeps the text untrusted and `trim` changes it in place, while `in` gives a plain bool. The reply joins the untrusted title, which `reply()` accepts. `release()` is not an action, so `max_per_hour: 30` counts only replies, and a question over the limit waits with its deferred event instead of reaching the model. Messages from other sessions' automations do not fire it, because `from_automations` defaults to `false`.

### status-beacon

Publishes a worker's status to `swarm.status` when it changes.

```rhai
let meta = #{
    name: "status-beacon",
    description: "Publish this worker's status when it changes, checked every 5 minutes",
    triggers: [#{ kind: "schedule", every: "5m" }],
    messaging: #{ publish: ["swarm.status"] },
};
let s = event.session;
let held = s.work.held;
let status = if held == () { s.status } else { s.status + " on " + held.group + "/" + held.work };
if status == state.last { return; }
state.last = status;
publish("swarm.status", s.name + " is " + status);
```

`session.work.held` names the work item the worker holds, if any. `state.last` remembers the status last published, and a firing that returns before acting costs nothing, so an unchanged status is free. A publication the history cannot record, or one over the publication rate, throws. The firing then commits nothing, and the next check publishes again. Recipients that refuse the message are listed in the receipt instead.

### task-tracker

Reacts to the outcomes of tasks this coordinator published.

```rhai
let meta = #{
    name: "task-tracker",
    description: "React to the outcomes of tasks this coordinator published",
    triggers: [#{ kind: "work_finished", groups: ["swarm-tasks"], states: ["completed", "failed", "paused"] }],
    limits: #{ max_per_hour: 30 },
};
if event.state == "completed" {
    message("A swarm task finished. Check its result, then publish follow-up tasks to swarm.tasks if any remain.", #{ attach: event });
} else {
    notify("Swarm task " + event.work + " is " + event.state + " after " + event.attempts + " attempts");
}
```

The coordinator's model publishes tasks to `swarm.tasks`, the topic of the `swarm-tasks` group, which hands each one to a worker. The user creates the group once with `caudra message group create swarm-tasks --topic swarm.tasks`. `work_finished` fires here, in the publishing session, when an item completes, fails, or pauses. A completed item queues a message with the event attached, because its `detail` is untrusted, and the other states notify the user. `work_finished` is a one-shot event, so `max_per_hour` defers a burst of outcomes rather than losing them.

### work-nudge

Asks the agent to report group work it paused without an outcome.

```rhai
let meta = #{
    name: "work-nudge",
    description: "Ask the agent to report group work it paused without an outcome, once per item",
    triggers: [#{ kind: "idle" }],
    limits: #{ max_per_hour: 6 },
};
let item = event.work.find(|w| w.pause_reason == "completion_required");
if item == () { return; }
if item.work == state.last {
    notify("Work item " + item.work + " in " + item.group + " paused again and needs you");
    return;
}
state.last = item.work;
message("Work item " + item.work + " in group " + item.group + " paused because your turn ended without an outcome. Finish it, then report with work_assignment.");
```

`idle.work` lists the work outcomes of the busy period that just ended. An item paused with `completion_required` waits for a person, though the agent that paused it can still report its outcome. The script asks once per item: `state.last` holds the item it asked about, so a second pause of the same item notifies the user instead. Items paused for another reason, such as `turn_limit`, are left to the user.

### research-desk

Answers research requests from other sessions with the `deep-research` workflow.

```rhai
let meta = #{
    name: "research-desk",
    description: "Answer research requests from other sessions with deep-research",
    triggers: [
        #{ kind: "message_received", audiences: ["direct"], consume: true },
        #{ kind: "workflow_finished", workflows: ["deep-research"] },
    ],
    workflows: ["deep-research"],
    messaging: #{ reply: true, send: ["*"] },
};
let requests = state.requests ?? #{};
if event.trigger == "message_received" {
    if event.sender == () || !event.text.starts_with("research:") { release("not a research request"); }
    let run = start_workflow("deep-research", #{ query: event.text.sub_string(9) });
    requests[run.run_id] = #{ to: event.sender, message: event.message_id };
    try {
        reply("Started " + run.name + ". The report follows when it finishes.");
    } catch (err) {
        log("Could not acknowledge the request: " + err.message);
    }
} else if requests.contains(event.run_id) {
    let request = requests.remove(event.run_id);
    send(request.to, "Report from " + event.name + " (" + event.status + ")\n\n" + event.report, #{ reply_to: request.message });
}
state.requests = requests;
```

The trigger consumes every direct message. A message from a script, which has no reply target, and any message that does not start with `research:` go back to the model through `release()`. A request starts a `deep-research` run with the rest of the text as its `query`, and `state.requests` maps the run's `run_id` to the sender and the request's `message_id`. The acknowledgement may fail, for example when the sender has left, so the script catches it and logs it: an uncaught failure would discard the record of a run that is already going. When the run finishes, the script sends the report to the sender as an answer to the request and removes the entry, so runs it did not start, and later finishes of the same run, send nothing. To report a resumed run's later finish too, remove the entry only on the status you treat as final, as section 8 explains. The untrusted report reaches `send()`, which accepts it. The query is peer text, and it loses its untrusted mark as a workflow arg, so `deep-research` must treat `query` as data and frame it for its agents, as section 7 explains. The script needs cross-session messaging and workflows turned on.

## 13. Common mistakes

| Symptom | Cause | Fix |
|---------|-------|-----|
| `` the first statement must be `let meta = #{ … };` `` | code above the header | Move the header to the top. Only comment lines may precede it. |
| `the header holds something other than an accepted literal` | a variable, call, or expression in the header | Use literals only. |
| `script failed to parse: 'default' is a reserved keyword` | `default:` as a map key, or `m.default` | Use `default_value` in arg specs. Elsewhere, quote the key: `#{ "default": 1 }` and `m["default"]`. |
| `Expecting '}' to end this object map literal` | another keyword as a key, such as `for:` | Rename or quote the key. |
| `` meta.triggers[0]: unknown field `delay`, expected `after` `` | an option the trigger does not take | Use the options in section 4. |
| `meta.triggers[0].after: expected a duration such as "2m"` | a duration string humantime cannot read | Write a string such as `"90s"`, `"2m"`, or `"1h 30m"`. |
| `needs exactly one of every and at` | a schedule with both or neither | Give one. `weekdays` goes with `at`. |
| `meta.args.goals: needs a default_value, because arm is "always"` | `arm: "always"` with a required arg | Give every arg a `default_value`, or let the user arm it by hand. |
| `the body reads args that meta.args does not declare: file` | a typo, or an arg without a spec | Declare the arg, or fix the name. |
| `the file name must equal meta.name`, in `list` | the file has another name | Rename the file to `<meta.name>.rhai`. |
| `Function not found: + ((), i64)` | `state.count += 1` before `count` exists | `state.count = (state.count ?? 0) + 1` |
| `Unknown property 'condition' - a getter is not registered for type '()'` | a field of a missing value, such as the goal when none is active | `event.session.goal?.condition ?? "none"` |
| `Function not found: contains ((), …)` | a misspelt field, read as `()` | Check the name against section 5. |
| `Data type incorrect: map (expecting bool)` | a map, or another value that is not a bool, as an `if` condition | Compare explicitly, as in `!= ()`. |
| `ended with untrusted …: text that becomes this session's instructions must be trusted` | untrusted text reached `message()` or `set_goal()` | Test it and act on the result, or pass it with `attach`. |
| `ended with placeholder` | `${value}` or `to_string` on an untrusted value | Join with `+` for `notify` and `log`, or use `attach`. |
| `` `+=` cannot add an untrusted value to plain text `` | `text += value` | `text = text + value` |
| `reserved keyword 'print' is disabled` | `print` or `debug` | `log(text)` |
| `log must be text, got i64` | a number given to `log` | `log("" + count)` |
| `Function not found: skip ()` | `skip` without a reason | `skip("why")` |
| `sleep() is unavailable in automation scripts` | waiting inside a firing | `after`, `cooldown`, or a `schedule` trigger |
| `the firing reached its limit of queued messages and goals` | more than 4 `message` or `set_goal` calls in one firing | Queue one message that lists the work. |
| `the firing reached its limit of operations` | an unbounded loop | Bound loops with a constant. |
| `delivery must be "next" or "guide", got "now"` | an unknown delivery | `"next"` or `"guide"` |
| `` message() has no option `deliver` `` | a misspelt option | `attach`, `delivery`, or `expires` |
| `Variable not found: event` | a `fn` that reads an outer variable | Pass the value as an argument. |
| `set_goal` fails with `goal_active` | another goal is active | Check `event.session.goal == ()` first, catch `goal_active`, or pass `replace: true`. |
| `http() may reach only the origins in meta.network, not https://api.example.com` | a `url`, or the value of a `url_env`, at an origin the header does not declare | Add the origin to `meta.network`. |
| `http() may read only the variables in meta.secrets, not GITHUB_TOKEN` | a variable in `url_env`, `bearer_env`, or `secret_headers` that the header does not declare | Add it to `meta.secrets`. |
| `http` fails with `refused`: `GITHUB_TOKEN is unset or empty` | the user has not set the variable | Tell the user to set it in their environment or the global `.env` file. |
| `http` answers 301, 302, 307, or 308 instead of the page | a redirect to another origin, which Caudra does not follow | Request the final URL directly and declare its origin in `meta.network`. |
| `needs experimental.cross_session_messaging`, in `list` | a messaging trigger or `meta.messaging` while cross-session messaging is off | Ask the user to set `[experimental] cross_session_messaging = true` in the global `caudra.toml` and restart. |
| `meta.triggers[0].consume: needs admissions: ["queued"]` | a consuming trigger that takes held messages | Keep the default `admissions: ["queued"]`. A held message waits for the user's review and cannot be consumed. |
| `reply() needs messaging.reply: true in the header` | `reply()` without the capability | Add `messaging: #{ reply: true }`. |
| `reply() and release() need an event that is a message this automation consumed, from a message_received trigger with consume: true` | `reply()` or `release()` on another trigger's event, or on a message the trigger did not consume | Set `consume: true`, and call them only when `event.consumed` is `true`. |
| `send() may reach only messaging.send, not @lead` | a target the header does not declare, or a name without its `@` | Add the `@name`, or a pattern such as `"@worker-*"`, to `meta.messaging.send`. |
| `publish() may use only messaging.publish, not ci.failures`, or `broadcast() needs "broadcast" in messaging.publish` | a topic or a broadcast the header does not declare | Add the topic, or `"broadcast"`, to `meta.messaging.publish`. |
| `reply` fails with `no_reply_target` | the consumed message came from a script | Check `event.sender == ()` first, and `release()` the message. |
| the model never sees some messages from other sessions | a firing on a consumed message completed without `reply()` or `release()` | `release()` every message the script does not answer. |
| `message_received` ignores another session's automation | `from_automations` defaults to `false` | Set `from_automations: true`, and give the exchange a way out. |
| `message_received` ignores a script's message | the `auto` inbound policy holds script messages for review | Add `"held"` to `admissions`, or ask the user for the `accept` policy. |
| `work_finished` never fires | another session or a script published the work, or it finished before the automation was armed | Arm the automation in the publishing session before it publishes. |
| `needs experimental.workflows`, in `list` | a `workflow_finished` trigger or `meta.workflows` while workflows are off | Ask the user to set `[experimental] workflows = true` in the global `caudra.toml` and restart. |
| `meta.workflows[0]: expected a kebab-case workflow name of at most 64 bytes` | a name such as `"review_changes"`, here or in a trigger's `workflows` | Use the workflow's kebab-case name, as its file name has it. |
| `start_workflow() may start only meta.workflows, not deep-research` | a workflow the header does not declare | Add the workflow's name to `meta.workflows`. |
| `start_workflow` fails with `refused` | the workflow is not approved, unknown, or invalid, 4 runs are active, or background admission is closed after Esc Esc | Ask the user to approve a project workflow in `/workflows`. Catch `refused` when the cap may be full, and try again on a later firing. |
| `start_workflow` fails with `invalid_argument` | an `agent_budget` that is not a positive integer, or is over the most a run may admit | Pass a smaller positive budget, or leave it out for the default. |
| `workflow_finished` reports `failed` right after a start | `args` the workflow does not expect, which a start never checks | Read the workflow's script for the args it reads, and pass those, with their types. |
| `workflow_finished` never fires | the run finished while the session was closed, or a shutdown interrupted it, or `workflows` names a run's display name instead of the workflow's name | Keep the session open while the run works, and filter on the workflow's name, the event's `workflow`. |
| a run is reported twice | a run that is resumed and finishes again fires again, under the same `run_id` | Remove the run's entry from `state` on the status you treat as final, or act on `event.status`. |
| the session loops on `idle` | a message on every idle without a way out | Add a stop marker, check `event.outcome`, and lower `max_per_hour`. |
| events show `rate_limited` | `max_per_hour` or `cooldown` refused a recurring event | Raise the limit, or act less often. |
| a project script cannot be armed | it is not trusted, or it changed since | Ask the user to trust it in `/automations`. |

## 14. Checklist before you hand it over

- The header comes first, holds only literals, and its `name` equals the file name.
- No map key is a Rhai keyword, and arg defaults use `default_value`.
- Every trigger that can start a turn has a way out: a stop condition, an `event.outcome` check, and limits sized to the work.
- The text of `message()` and `set_goal()` is built from literals, args, and trusted fields. Untrusted values reach the model only through `attach`.
- Values that may be missing are read with `?.` and `??`.
- Every origin `http()` reaches is in `meta.network`, every variable it reads is in `meta.secrets`, and the answer names the variables the user must set.
- Every `@name` and topic the body messages, and every `reply()`, is declared in `meta.messaging`.
- A firing on a consumed message answers it with `reply()` or hands it back with `release()` on every path, unless keeping it from the model is the point.
- A trigger with `from_automations: true` has a way out, and a script that reacts to group work keys what it records on the work name.
- Every workflow `start_workflow()` launches is in `meta.workflows`, untrusted args go only to a workflow that treats its args as data, and a loop that starts runs from `workflow_finished` is bounded by `max_per_hour` and `state`.
- A script that answers for each run maps `run_id` in `state`, and handles a resumed run that fires again under the same `run_id`.
- `state` records what a firing did in the same firing, and correlates by value.
- Every `skip` has a reason that will make sense in `history`.
- `validate` passed, and the directory came from `list`.
- The answer ends with the exact `/automations arm` or `--automation` line, args included, and tells the user to trust a project script in `/automations` first.
- For a messaging script, the answer also says that cross-session messaging must be on, and gives the launch line with the session's name, topics, and groups.
- For a workflow script, the answer also says that workflows must be on, and names the workflows it starts or watches.
