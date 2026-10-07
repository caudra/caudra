+++
title = "Automations"
weight = 25
[extra]
group = "Guides"
+++

# Automations

An automation is a short Rhai script that reacts to events in one session. Its header names the triggers it waits for, such as the session going idle, a goal finishing, or a schedule coming due. When one fires, the body runs once with the event in scope, and that run is a firing. The body decides with plain `if` statements whether to act. It acts through a few host functions: it can queue a message for the model, set a goal, notify you, call an HTTP endpoint, message other sessions, or start a workflow.

Use one when a session should keep working while nobody types, act on a schedule, guard its own spending, or answer other sessions. For a single objective, a [completion goal](/docs/commands/#completion-goals) is enough. For one multi-agent plan that runs to a result, write a [workflow](/docs/workflows/). An automation can also start a workflow and act on its result.

Automations are experimental and off by default.

## Turn them on

Set `automations = true` under `[experimental]` in the global `caudra.toml`, then restart Caudra:

```toml
[experimental]
automations = true
```

Triggers and actions that reach other sessions also need `cross_session_messaging = true`, and those that start or watch workflows need `workflows = true`. Each switch is independent. A script that uses a feature that is off stays in the catalog as invalid, with a reason such as `needs experimental.workflows`. See [Experimental features](/docs/configuration/#experimental-features).

While the switch is off, Caudra offers no `/automations` command, `--automation` flag, `automation` tool, or status chip, and it arms nothing. Turning it off keeps the automations, args, and state that sessions saved.

The session limits live in the global [`[automations]`](/docs/configuration/#automations) table. The builtin `caudra-automation-dev` skill teaches the agent to write automations, and it is on by default while automations are on. Set `automation_dev = false` under [`[plugins.skill]`](/docs/configuration/#plugins-skill) to remove it.

## Arm one

A script does nothing until you arm it in a session. Arming takes args, so one script can serve many sessions with different goals, files, or commands.

- `/automations arm NAME` arms it in the current session, with the args as a JSON object after the name: `/automations arm goal-chain {"goals": ["The login tests pass"]}`. `/automations disarm NAME` disarms it, and Space in the [inspector](#the-inspector) does both.
- `--automation NAME` arms it in the session Caudra starts with. `NAME=JSON` gives the args inline and `NAME=@FILE` reads them from a file. Repeat the flag for more automations, naming each once.
- The `automations:` list in the frontmatter of a [system prompt profile](/docs/system-prompts/) arms each entry when a session starts with the profile or switches to it. An entry is a name, or a map with `name` and `args`. Switching to a profile that lacks an entry disarms what the previous profile armed.
- `arm: "always"` in the header of a user script arms it in every session as the session starts. Every arg then needs a `default_value`. Project scripts cannot use it.
- SDK clients send `automation_arm`. See [Headless Mode](/docs/headless/#automations).

A profile that arms two automations, one of them with args:

```yaml
---
automations:
  - join-swarm
  - name: goal-chain
    args: {goals: ["The login tests pass", "The signup tests pass"]}
---
```

Arming checks the args against the script's declarations. A missing required arg, an undeclared name, or a value of the wrong type refuses the arming with the reason.

Args are saved with the session. Resuming it arms each automation again with its stored args and fires `armed` with reason `resume`. A profile's args only seed the first arming, so args you changed later survive. `--automation NAME=…` replaces the stored args, and a bare `--automation NAME` keeps them. An automation you disarmed stays disarmed on resume, even when a profile or `arm: "always"` lists it, until you arm it again. When a script edit makes the stored args invalid, the automation stays disarmed until you fix them in the inspector.

## Where scripts live

| Scope | Location | Trust |
|-------|----------|-------|
| Project | `<project root>/.caudra/automations/<name>.rhai` | per exact SHA-256 digest |
| User | `~/.config/caudra/automations/<name>.rhai` | trusted as written |

Debug builds use `~/.config/caudra-debug/automations/` instead, following the platform directory rule in [Configuration](/docs/configuration/#directory-layout). A session on a remote workspace or a sandbox loads user scripts only, because its project lives on another machine.

The file name without `.rhai` must equal `meta.name`. A script is a regular UTF-8 file of at most 64 KiB, and a symlink is refused. A file that breaks a rule stays in the list as invalid, with the reason. Caudra reads both directories again whenever it lists the catalog, arms a script, or starts a session, so a fixed file shows up without a restart. When both scopes hold the same name, the project script hides the user script.

A project script arms only after you trust it. Select it in `/automations` and press `t`, which shows the digest you approve. The digest is SHA-256 over the exact bytes together with the automation language and host ABI versions, so changing one byte makes the script untrusted again until you review it. Neither the model nor a script can grant trust.

## Write one

A script is a header and a body:

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

The header is the first statement, `let meta = #{ … };`. Caudra reads it without running the script, so it holds only literals. It names and describes the script, lists one to eight triggers, and declares the limits, the args, and what the body may reach: `network` origins and `secrets` for `http()`, `messaging` targets, and `workflows`. A call outside those declarations stops the firing.

The body is ordinary Rhai that runs top to bottom once per firing, with `event`, `state`, `args`, and `meta` in scope. Conditions are plain `if` statements. `skip(reason)` ends a firing without acting and records why, and `return` ends it early. Actions are calls to host functions.

The agent can write one for you. The builtin `caudra-automation-dev` [skill](/docs/skills/#the-builtins) holds the full reference, from every header key, event field, and host function to the parts of Rhai that trip people up. The agent writes the file, validates it with the [`automation` tool](/docs/tools/#automation), and gives you the line that arms it. Only you can trust and arm it.

### Triggers

| Kind | Fires when | Options |
|------|------------|---------|
| `armed` | the automation is armed: at launch, on resume, when you arm it again, and when the pause latch clears | none |
| `idle` | the session settles after a busy period and stays settled for `after` | `after` |
| `needs_input` | the session has waited on you for `after`, such as at a permission prompt, a question, or a plan | `after`, `inputs` |
| `goal_finished` | the session goal ends as met, impossible, or cleared by an error | `verdicts` |
| `schedule` | an occurrence comes due, `every` a period or `at` a time of day | `every` or `at`, `weekdays`, `catch_up` |
| `message_received` | a message from another session or a script reaches this session | filters and `consume`, under [Messaging](#messaging) |
| `work_finished` | consumer-group work this session published reaches an outcome | `groups`, `states` |
| `workflow_finished` | a workflow run in this session finishes | `workflows`, `statuses` |

`idle` and `needs_input` fire once per transition, and the opposite edge cancels a pending `after`, so `after: "2m"` waits for two quiet minutes. `every` is at least one minute, counted from arming. `at` takes `"HH:MM"` in the script's `timezone`, and `weekdays` limits it to the days you list. Missed occurrences, such as those while the session was closed, collapse into the latest one. `catch_up: "once"`, the default, fires it late, and `catch_up: "skip"` drops it.

### Actions

| Function | What it does |
|----------|--------------|
| `message(text)` | Queues trusted text for the model. `delivery: "guide"` joins a running turn, `attach` adds untrusted data, and `expires` drops a message that waited too long |
| `set_goal(condition)` | Sets the session goal and queues its kickoff turn. `replace: true` replaces an active goal |
| `notify(text)` | Flashes the text in the status bar and sends it as a [terminal notification](/docs/notifications/). SDK clients receive it as an `automation_notice` message |
| `http(request)` | Sends one request to an origin in `meta.network`, with credentials from the variables in `meta.secrets` |
| `send`, `reply`, `publish`, `broadcast` | Message other sessions, as [Messaging](#messaging) describes |
| `release(reason)` | Ends the firing and hands a consumed message back to the model |
| `start_workflow(name, args)` | Starts a workflow run, as [Workflows](#workflows) describes |
| `pause_automations(reason)` | Sets the session's pause latch, as Esc Esc does |
| `skip(reason)` | Ends the firing without acting and records why |
| `log(text)` | Adds a line to the firing's trace |
| `now()` | Returns the time in the script's time zone |

Messages wait in an outbox until the session can take them, and the model receives each one under a header that names the automation. `http()` sends each request once, without retries, and every status comes back as an answer. The script names environment variables for its secrets and never sees their values, and the history keeps only the names. Caudra reads those variables from its own environment and the global `.env` file. A project `.env` cannot set them. Loopback and private hosts stay out of reach unless the global config sets `allow_private_network = true`.

A call the header does not allow, untrusted text in `message()` or `set_goal()`, or a per-firing limit stops the firing at once, and `try` cannot catch it. Other failures, such as an active goal or an unknown recipient, can be caught with `try` and `catch`. A firing that fails or stops keeps none of its state changes, and effects it already caused stay.

### Untrusted text

Text from outside the script arrives marked untrusted: the session title, the model's last response, error messages, goal reasons, messages from other sessions, workflow reports and results, and HTTP responses. Joining it to other text keeps the mark.

`message()` and `set_goal()` refuse untrusted text, because their text becomes this session's instructions. Without the mark, another session's message or a web page could steer your model through the script. Every other host function accepts untrusted text.

A script can still act on what the text says:

- Tests such as `==`, `contains`, `starts_with`, `matches`, and `len` return plain values, so `if event.last_response.contains("BLOCKED")` works.
- `one_of(value, ["done", "blocked"])` returns the matching string from the script's own list.
- `parse_int` and `parse_float` return plain numbers. `parse_json` returns an untrusted structure whose numbers and booleans are plain.
- The `attach` option of `message()` shows an untrusted value to the model as a framed JSON block after the trusted text. It is the only way to show such text to the model.

Interpolating an untrusted value with `${…}` gives a placeholder that every host function refuses. The args you give when arming are trusted. Args passed to `start_workflow()` lose the mark, because workflow args are data, so pass outside text only to a workflow that treats its args as material for its agents.

### State and args

`state` is a map the automation keeps in one session, empty until a firing commits something. A firing commits its changes when it completes: it runs to the end, returns, skips, or releases. A firing that fails, is stopped, or is cancelled commits nothing. One firing per automation runs at a time, so firings never race on `state`. It holds at most 64 KiB, keeps untrusted values marked, and survives disarming, new args, and script edits.

Each commit raises the state's revision, and a write applies only at the revision it read. When you save an edit while a firing runs, the firing loses: it commits nothing, and its trace says so. When a firing commits while you edit, your save is refused, and the editor stays open and names the newer revision.

Args are typed. A script declares each one as a `string`, `int`, `float`, `bool`, or `list` of strings, with an optional `default_value`, `min` and `max`, `choices`, and a description. An arg without a default is required. Args cannot change the header, so triggers, limits, and capabilities stay as the script wrote them.

## Limits and safety

Automations start turns while nobody watches, so limits apply to each firing, each automation, and each session.

**Each firing** may use at most 1 million operations, 120 seconds of wall time including host calls, 32 actions, 64 log lines, and 4 calls to `message` and `set_goal` together. Reaching any of them stops it.

**Each automation** takes `max_per_hour` and `cooldown` from the `limits` in its header. `max_per_hour` counts the firings that act per rolling hour, 12 by default and 600 at most. `cooldown` is the shortest gap between two of them, none by default and 24 hours at most. A firing acts when it calls a host function other than `now`, `log`, `skip`, `release`, `pause_automations`, and the parsers. Its first action checks the limits, so a firing that only reads, logs, or skips costs nothing. When a limit refuses, the firing stops before acting and keeps no state. What happens to its event depends on the trigger:

- `armed`, `goal_finished`, `message_received`, `work_finished`, and `workflow_finished` happen once. Their event is deferred and retried once the limit allows, and later events wait behind it.
- `idle`, `needs_input`, and `schedule` recur. Their event is recorded as `rate_limited` and not retried.

After a failed firing, the automation's next acting firing waits 1 minute, doubling up to 30 minutes. A firing that acts and completes resets this backoff, and so does arming again.

Each automation queues at most 16 events and runs one firing at a time. A newer `armed`, `idle`, `needs_input`, or `schedule` event replaces a waiting one of its kind, and a full queue drops its oldest event. A session runs at most 4 firings at once.

**Each session** has limits that cover all of its automations together. Only the global config can set them:

| Key | Default | Effect |
|-----|---------|--------|
| `turns_per_hour` | `20` | Turns automations may start per rolling hour. Messages wait in the outbox until there is room |
| `max_unattended_turns` | unset | An optional cap: automation-started turns stop after this many since your last prompt |
| `allow_private_network` | `false` | Lets `http()` reach loopback and private hosts |

When automation-started turns keep ending in error, the next delivery waits 1 minute, doubling up to 30 minutes. A clean run or your next prompt resets it. Turns an automation starts run in the session's current mode and permission mode.

**The pause latch** stops every automation in the session. These set it: Esc Esc while the session works, Ctrl+C while a reply streams, cancelling a question form, an SDK interrupt, `p` in the inspector, and a script's `pause_automations()`. While it holds, running firings are cancelled, later events are recorded as `paused` without running, and triggers consume no messages. Your next prompt clears it, and so does `p`. Clearing it fires `armed` with reason `unpaused`, so a script can pick up where it stopped. The latch is saved with the session.

## The inspector

`/automations` opens the inspector, and so does a click on the status bar chip. The list on the left holds a row for the session, then this session's catalog grouped as Armed, Available, Needs trust, and Invalid, then [other sessions](#other-sessions). An automation row shows its status, its scope, and how long ago it last fired and how that firing ended. `/` filters the list.

The right pane shows the sections of the selection:

| Selection | Sections |
|-----------|----------|
| The session | Overview: the pause latch and who set it, the turns this hour and the next free slot, unattended turns, the delivery backoff, and what keeps the session from settling. Firings: those of every automation, merged. Outbox |
| An automation | Overview: description, scope, path, digest, trust, how it was armed, triggers with their next due times, limits and their use, the failure backoff, and capabilities. Firings, State, and Args |

**Firings** are grouped as Waiting, Running, and Finished. A row shows the time, the trigger, the status and duration, and a one-line summary: the first action, the skip or release reason, or the error. Quiet skips with the same trigger and reason merge into one row with a count. Enter opens a firing as a trace: the event as a JSON tree with untrusted text marked, each action in call order with its source line and status, the error with the line that raised it, and the state change. A queued message shows why it waits, such as a full `turns_per_hour` or an open modal, and later the outcome and cost of the turn it started. Enter on an action opens its request and result.

**The outbox** lists the messages and goals that automations queued for the model, each with why it waits. `x` drops one.

**State** shows the committed state as a JSON tree, with untrusted values wrapped as `{"$untrusted": …}`. **Args** lists each declared arg with its type, default, description, and current value. `e` edits either one as JSON. A save checks the shape and the size and applies with the revision check described in [State and args](#state-and-args). Saving args arms the automation again. Removing an `$untrusted` wrapper marks that value trusted, a choice only a person can make.

The other keys arm and disarm, trust a project script at the digest shown, pause, clear state, drop a waiting firing, open the script at a failing line, and copy a firing as Markdown. See [Keybindings](/docs/keybindings/#context-specific).

The status bar shows `[auto · N]` while N automations are armed. A failed firing turns it red with a failure count until you open the inspector, and flashes a notice such as `goal-chain failed at line 12 (/automations)`, at most once a minute per automation. A message an automation delivered has its own row in the transcript, and clicking it opens its firing.

### Dry runs

`r` on a finished firing of this session runs its event again against the script file as it is on disk now, so you can try an edit before you arm it again. The run uses the session's current args, or else the stored or default ones, and a copy of the current state. `now()` returns the time of the original firing.

Deliveries, messages, releases, notifications, and pauses are recorded instead of performed. `start_workflow()` returns a placeholder run id. `http()` answers from the original firing's journal when the request matches, and otherwise gets the stub `#{ status: 0, body: "", json: () }`. Each action in the result is badged `recorded`, `journal`, or `stubbed` by how it was answered. A journaled result that was too large to store gets the stub too, badged `cut`.

Capability checks and per-firing limits apply. A dry run reports what the automation limits would have done, runs on regardless, and spends none of their allowance. It needs no trust, because it performs nothing.

The result appears at the top of Firings with a `dry run` badge until you select another row in the list or close the inspector. Dry runs are never stored. Enter opens the result like a firing, with the state change it would have committed and the state revision it ran against, and a note says when the script changed since the original firing. A firing whose event was cut for storage, or that matches no trigger of the current script, cannot be replayed, and the row says why. SDK clients send `automation_dry_run`, as [Headless Mode](/docs/headless/#automations) describes.

### Other sessions

Below this session's catalog, the list shows the other sessions that have an armed automation or a firing in the last 7 days, newest activity first, at most 50. Each shows its title and its `@name`, which stays known while the session is offline. With cross-session messaging on, sessions in the live peer directory are marked online.

Selecting one of their automations shows its Overview, Firings, and State, read again every 5 seconds while it stays selected. The view is read-only, with no controls, editors, or dry runs. Enter and copy work as they do for this session. `/` filters by name, description, and session title or `@name`.

An `--ephemeral` session keeps its own database. Other sessions never list it, and its inspector lists no other session.

## Messaging

With cross-session messaging on, an automation can take part in a group of sessions on this machine. [Cross-session messaging](/docs/messaging/) covers names, topics, broadcasts, and consumer groups.

`message_received` fires when a message from another session or a script reaches this session. Its options filter by audience, topic pattern, sender `@name`, script label, and whether the message was queued or held for your review. By default it ignores what other sessions' automations send, so two automations cannot keep each other going. `from_automations: true` lifts that.

With `consume: true`, the trigger takes each message it matches from the model and hands it to the automation alone. The message goes back to normal delivery when the firing calls `release()`, fails, or is stopped, and when its event is dropped. A firing that completes without releasing keeps the message from the model for good. Only messages already admitted as queued are consumed, so a held message still waits for your review.

Sending needs the `messaging` capabilities in the header:

| Function | Sends | Needs |
|----------|-------|-------|
| `reply(text)` | an answer to the sender of the consumed message | `reply: true` |
| `send(to, text)` | a direct message to an `@name` | a matching entry in `send` |
| `publish(topic, text)` | a message on a topic, which also queues work in the consumer groups on it | the topic in `publish` |
| `broadcast(text)` | a message to every session that receives broadcasts | `"broadcast"` in `publish` |

A message goes out as the session, marked as the automation's. Recipients see `sender_kind: "automation"` and the automation's name, and their inbound policy judges it as it judges the session's own messages. An automation can send while the session waits on you, or after a failed run stopped its automatic wakes. The rate limits and group limits apply, a ReadOnly session cannot send, and the message history records every message.

`work_finished` follows the consumer-group work that this session published, from its model and its automations alike. It fires when an item completes, fails, or is cancelled, and when it pauses if `states` includes `"paused"`. `event.session.work` shows the item the session holds and the items it paused that still wait for an outcome.

## Workflows

With workflows on, an automation can start runs and act on their results. `meta.workflows` lists the workflows that `start_workflow(name, args)` may start. The run belongs to the session and goes on in the background, and the call returns its `run_id` and display name. A session holds at most 4 active runs, shared with the model's `workflow` tool. A project workflow still needs its own approval in `/workflows`. Esc Esc closes background admission until your next prompt, and a start fails until then.

`workflow_finished` fires when a run in this session reaches a final status, whoever started it, and carries its report and result as untrusted values. A run that finishes while the session is closed does not fire. Every completion also starts a model turn, as it does without automations. See [Workflows](/docs/workflows/).

## Storage

Automations keep their data in `caudra.db` in the [state directory](/docs/configuration/#directory-layout), and each session holds its own:

- the binding of each automation the session armed, with how it was armed, its args, its state, and the marks of its limits and schedules
- the text of each script version a firing ran, so a trace shows the right lines after an edit
- each firing, with its event, its actions, and the state change it committed

Each automation keeps its newest 100 finished firings per session, and waiting ones are never trimmed. A firing stores at most 256 KiB, and its event at most 64 KiB. Past that, a body keeps a short preview and the trace says so.

Resuming a session restores its bindings, state, waiting events, and outbox, so the inspector shows the same history. Firings that were running when Caudra stopped end as `interrupted`. Waiting `armed`, `idle`, `needs_input`, and `schedule` events are dropped, because resume fires `armed` again and schedules catch up. Schedules, limit windows, and the `work_finished` cursor keep their marks, so a restart repeats no schedule occurrence or work outcome.

A [fork](/docs/sessions/#fork-boundaries) starts with no automations, args, or state. [Trimming](/docs/sessions/#retention) a session deletes its firings, actions, and script versions, and keeps its bindings and state. [Moving a session](/docs/sessions/#moving-sessions-to-another-directory) to another directory is refused while a firing runs. The move interrupts waiting firings and disarms project scripts, because they belong to the old project. An [ephemeral session](/docs/sessions/#ephemeral-sessions) keeps its own `caudra.db`, which goes away at exit with everything in it.

## Other frontends

SDK sessions run automations too. The client arms and inspects them with controls, and firings arrive as `system` messages. An SDK session shows no prompt to wait on and takes no part in cross-session messaging, so a script with a `needs_input`, `message_received`, or `work_finished` trigger, or with `meta.messaging`, is invalid there. See [Headless Mode](/docs/headless/#automations).

One-shot `--print` and ACP sessions run no automations, and both refuse `--automation`.

## Patterns

These scripts are the examples Caudra's test suite replays against scripted events, ordered from one session on its own to a swarm. A script that needs more switches than `automations` to load names them.

<!-- caudra-docgen:automation-patterns -->

### retry-overload

Resume after rate-limit and overload errors.

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

### keep-going

Work through a backlog file during work hours.

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

### timebox

Ask a turn that has worked for an hour to wrap up.

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

### goal-chain

Pursue a list of goals in order, starting with the first when armed.

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

### standup

Write standup bullets at 09:00 on weekdays and post them to Slack.

```rhai
let meta = #{
    name: "standup",
    description: "Write standup bullets at 09:00 on weekdays and post them to Slack",
    triggers: [
        #{ kind: "schedule", at: "09:00", weekdays: ["mon", "tue", "wed", "thu", "fri"], catch_up: "skip" },
        #{ kind: "idle" },
    ],
    network: ["https://hooks.slack.com"],
    secrets: ["SLACK_STANDUP_URL"],
    timezone: "Europe/Berlin",
};
if event.trigger == "schedule" {
    message("Summarize yesterday's commits in this repository as three standup bullets. Reply with only the bullets.");
} else if (meta.name in event.automations) && event.outcome == "completed" {
    http(#{ method: "POST", url_env: "SLACK_STANDUP_URL", json: #{ text: event.last_response } });
}
```

### spend-guard

Pause automations once this session has spent $20.

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

### page-me

Push a phone notification when the session has waited on me for 10 minutes.

```rhai
let meta = #{
    name: "page-me",
    description: "Push a phone notification when the session has waited on me for 10 minutes",
    triggers: [#{ kind: "needs_input", after: "10m" }],
    network: ["https://ntfy.sh"],
    secrets: ["NTFY_URL", "NTFY_TOKEN"],
    arm: "always",
};
let ask = switch event.input {
    "permission" => "approve " + event.tool,
    "plan" => "review a plan",
    "auth" => "sign in again",
    _ => "answer a " + event.input,
};
http(#{
    method: "POST",
    url_env: "NTFY_URL",
    bearer_env: "NTFY_TOKEN",
    headers: #{ Title: "Caudra is waiting" },
    body: event.session.title + " needs you to " + ask,
});
```

### goal-webhook

Post goal outcomes and record blockers when a goal is impossible.

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

### nightly-review

Review the day's commits at 02:00 and post the report to Slack.

Needs `experimental.workflows`.

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

### ci-watch

Poll GitHub Actions on main and publish new failures to ci.failures.

Needs `experimental.cross_session_messaging`.

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

### join-swarm

Announce this worker on swarm.status when it starts or resumes.

Needs `experimental.cross_session_messaging`.

```rhai
let meta = #{
    name: "join-swarm",
    description: "Announce this worker on swarm.status when it starts or resumes",
    triggers: [#{ kind: "armed" }],
    messaging: #{ publish: ["swarm.status"] },
};
if !(event.reason in ["launch", "resume"]) { skip("armed by hand"); }
publish("swarm.status", event.session.name + " is online");
```

### status-beacon

Publish this worker's status when it changes, checked every 5 minutes.

Needs `experimental.cross_session_messaging`.

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

### status-desk

Answer status questions from other sessions without a model turn, and pass every other direct message to the model.

Needs `experimental.cross_session_messaging`.

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

### work-nudge

Ask the agent to report group work it paused without an outcome, once per item.

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

### task-tracker

React to the outcomes of tasks this coordinator published.

Needs `experimental.cross_session_messaging`.

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

### ci-triage

Start a root-cause run for each CI failure, instead of waking the model.

Needs `experimental.cross_session_messaging` and `experimental.workflows`.

```rhai
let meta = #{
    name: "ci-triage",
    description: "Start a root-cause run for each CI failure, instead of waking the model",
    triggers: [#{ kind: "message_received", topics: ["ci.failures"], senders: ["@ci-watcher"], scripts: ["nightly-ci"], consume: true }],
    workflows: ["root-cause"],
    limits: #{ max_per_hour: 2 },
};
start_workflow("root-cause", #{ failure: event.text }, #{ agent_budget: 24 });
```

### research-desk

Answer research requests from other sessions with deep-research.

Needs `experimental.cross_session_messaging` and `experimental.workflows`.

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

<!-- /caudra-docgen:automation-patterns -->
