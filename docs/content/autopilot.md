---
title: "Autopilot"
description: "Keep a session moving toward its goal while you are away."
---

Autopilot answers what would otherwise wait for you, so a session keeps working toward its goal unattended. It answers permission prompts, the `question` tool, questions the assistant asks in prose, and a plan that is ready to implement. It also reviews the stalls that would end a run, such as a goal loop cap or a provider outage.

Each answer is a separate model call, the *delegate*, in the way the [goal evaluator](/docs/commands/#completion-goals) judges a goal. The delegate sees your own words, the goal, a bounded tail of the conversation, and the item it decides. It never runs tools. When it cannot decide safely, it leaves the item to you and tells you.

Use autopilot with a [completion goal](/docs/commands/#completion-goals) when a task should run to the end without you. [Automations](/docs/automations/) decide *when* a session acts. Autopilot keeps the session from stopping once it does.

## Engage it

Only you engage autopilot. Scripts, profiles, and the model cannot.

| How | What it does |
|-----|--------------|
| `/autopilot` | Engages without a goal. On an engaged session it opens the status panel. |
| `/autopilot <goal>` | Sets the goal as `/goal <goal>` does, and engages with autopilot owning it. On an engaged session it replaces the goal. |
| `/autopilot off` or `/autopilot stop` | Disengages. |
| `/autopilot model` | Picks the model for the Autopilot job. |
| `/autopilot log` | Opens the [log](#the-log) of what autopilot did in this session, engaged or not. |
| `--autopilot` | Engages the session Caudra starts with, in the terminal UI, `-p`, and the SDK. |
| `autopilot_start` | Engages from an SDK client. See [Headless Mode](/docs/headless/#autopilot). |

Engaging resets the budget and tells the model, through a `# Autopilot` reminder, that answers may come from a delegate while you are away. The reminder asks it to ask only when the answer changes the outcome, and to ask through `question` where it can.

### Before you leave it alone

Autopilot engages even when something it relies on is missing. The `Autopilot engaged` row is then followed by a notice that says what to do:

| Notice | What it means | What to do |
|--------|---------------|------------|
| No decision endpoint is active | Nothing screens fetched pages or tool output, so injected text can lead autopilot to approve calls. | Add an endpoint with `/login`, or select one with `caudra decisions use <name>`. Then turn on content screening. |
| Content screening is off | An endpoint is active, and [`content_screening`](/docs/configuration/#decisions-features) is `off`. | Turn it on in `/decisions`. |
| Autopilot's model cannot be used | The model bound to the Autopilot job has no provider or no credentials. A bound model is never replaced, so every item is left to you. | Pick another with `/autopilot model`, or sign in to its provider. |

Caudra checks the delegate's model and its credentials without calling the model. The notices change nothing about the engagement, and they are not saved in the [log](#the-log). In `-p` they go to stderr, and SDK clients receive them as `caveat` events. See [Headless Mode](/docs/headless/#autopilot).

### Owned goals

Autopilot owns a goal only when engaging set it: `/autopilot <goal>`, `autopilot_start` with a goal, or `-p --autopilot` with a `/goal <condition>` prompt. Without an owned goal, it keeps whatever goal is active moving. With no goal at all, it only keeps the session from stalling.

A goal you set by hand while autopilot is engaged is not owned, and neither is a goal an automation sets. Replacing or clearing an owned goal by hand drops the ownership, and autopilot stays engaged.

### When it ends

Autopilot ends when:

- you stop it,
- its owned goal is met, or the evaluator confirms it impossible,
- a [budget](#budgets) runs out,
- an unrecoverable error occurs, or retries of a provider error run out.

When a goal that autopilot does not own finishes, autopilot keeps running. Messages you type leave it engaged, and it carries on around them. Every end is a transcript row and a notification, and the model is told that you answer again.

## What it answers

### Permission prompts

When a prompt appears, the delegate decides it. In [Auto mode](/docs/permissions/#auto-mode), Auto screens the call first, as usual, and the delegate decides only what Auto leaves. YOLO never prompts.

| Prompt | What the delegate may do |
|--------|--------------------------|
| Any prompt outside the classes below, including one an ask rule raised | Allow once, deny with guidance, or leave it to you |
| Prompts that plan mode raises | Deny with guidance, or leave it to you |
| Tools and calls that always prompt | Deny with guidance, or leave it to you |
| Protected files and commands, such as `.git`, `.ssh`, dotenv files, and file redirects | Deny with guidance, or leave it to you |
| `sudo`, `su`, `doas`, `eval`, `source`, and command lines Caudra cannot read | Deny with guidance, or leave it to you |
| Any prompt while content screening has flagged the session | Deny with guidance, or leave it to you |

These are the classes that [Auto](/docs/permissions/#auto-mode) never decides, except ask rules. An ask rule you wrote reaches the delegate, which may approve that call once. Use a deny rule for a call that must never run.

An approval is always for this call only. The delegate never adds a rule or a lifetime grant. A denial reaches the model as "Autopilot guidance:" followed by the reason, so the model can choose another way.

The delegate decides a permission prompt without tool results. It sees your words, the assistant's own text and calls, and the request, including the tool input, its resources, why it asks, and any engine advice. Text planted in a fetched page or a file therefore stays one step away from a grant.

The flag that blocks grants comes from [content screening](/docs/permissions/#decision-engine-advice), which needs the decision engine. Without the decision engine, there is no such flag.

Prompts from subagents, workflow agents, and background tasks reach the delegate too, because they share the session's permissions. A permissions reload while the delegate decides cancels the decision, and the prompt is asked again under the new rules.

With no one to answer, as in `-p`, the delegate still decides, and a prompt it leaves to you is denied with "Autopilot left this for a human, and none is present."

### Questions

The delegate answers the `question` tool form, with one answer for each question. An answer may pick listed options or give its own text. The answer reaches the model marked as autopilot's, and it never counts as your requirement. It answers a [deferred question](/docs/sessions/#deferred-questions) the agent asked with `background: true` the same way, even after the run that asked it ends. A question you put off with `F4` is yours, and the delegate leaves it to you.

The delegate also answers a form an MCP server shows during the main agent's tool call (see [Server questions](/docs/mcp/#server-questions)). It never answers a request to open a link, a form from Executor, a form with no fields, a form with a field that looks like a credential, or a form from a subagent's call. Those wait for you. The card lists each answer as autopilot's.

When the assistant asks in prose instead, two steps run in order at the end of the turn:

1. The [question-tool nudge](/docs/configuration/#question-tool-nudge) runs first, unchanged. When it fires, the assistant asks again through `question`, and the delegate answers the form.
2. Where the nudge did not fire, autopilot checks whether the reply asks you something. If it does, the delegate replies, says the work is done, or leaves it to you.

The nudge fires at most once for each message you type, and autopilot answers never renew that allowance. With the nudge in `advise`, most prose questions in the terminal UI become forms, which the delegate answers precisely. That costs one extra model turn each time the nudge fires.

### Plans

In plan mode, after the assistant writes the plan, the delegate reviews it before anything else runs. It can approve it, send revisions, or leave it to you.

Approving switches to build mode and starts a turn that implements the plan, as approving the plan card does. The implement message is autopilot's, and it resets the [stalled-work count](/docs/context/#stalled-work-after-compaction) as your approval would.

### Stalls

| Stall | Without autopilot | With autopilot |
|-------|-------------------|----------------|
| Goal loop cap | The run ends and the goal stays | The delegate reviews the progress and continues with guidance, or leaves it to you |
| A goal makes no progress for three turns in a row | The run ends and the goal stays | The same review. A continue buys one more turn, so the next turn without progress is reviewed again |
| A goal waits on a change outside the session | The run ends and the goal is checked again after 1, 2, 4, 8, and 16 minutes, then every 30 minutes, for eight waits | No review until the waits run out, which is the stall above |
| `max_turns` reached | The run ends | The same review |
| Steering exhausted on unusable output | The run fails | The same review |
| The evaluator says the goal is impossible | The goal finishes | The delegate gives one second opinion for each goal. It agrees, or continues the work with guidance |
| A provider error that stays after the usual retries | The run fails | The run retries after 1, 2, 4, 8, and 16 minutes, then every 30 minutes, up to `error_retries`, each with a fresh retry budget. Calls that already ran never run again. When the retries run out, autopilot ends |
| An unrecoverable error, such as a rejected request | The goal is cleared and the run fails | Autopilot ends |
| Group work taken and not reported | The run ends | One reminder for each item asks the agent to report it with `work_assignment` |

Three reviews in a row that continue, with no input from you between them, are the limit. The fourth review is left to you, so a stuck loop stops before the budget does.

When a review continues, the stall's own notice does not appear, and the decision row takes its place. When the review leaves it to you, you see what you would see without autopilot.

Provider sign-in that needs you still waits for you, with an urgent notification.

## Budgets

Each engagement has its own budget, set in the global [`[autopilot]`](/docs/configuration/#autopilot) table:

| Key | Default | Counts |
|-----|---------|--------|
| `max_continuations` | `100` | Turns that start without you while autopilot is engaged. `0` removes the limit |
| `max_cost` | unset | API-billed spend since engaging, in USD |
| `max_minutes` | unset | Minutes since engaging while the session is open |
| `error_retries` | `5` | Retries of one provider error |

A continuation is any turn that starts without you: goal continuations, delegate replies, revisions and guidance, retries, todo and question-tool reminders, the unreported-work reminder, the implement turn, and the check-in after a resume. The turn that follows a [compaction](/docs/context/) does not count.

Spend covers the session's own turns, its subagents, the goal evaluator, and the delegate, wherever the price is known. Subscription spend is shown in the status panel and is never capped.

When a budget runs out, autopilot ends and the goal stays active.

With `max_continuations = 0`, an engagement can run for days, for example in a session that an [automation](/docs/automations/) gives a goal every morning. The footer chip then counts continuations without a limit. `max_cost`, `max_minutes`, and `error_retries` still end it. Each goal's own continuation limit still stops its loop, and autopilot reviews that stall as usual. Only your input breaks the [review streak](#stalls), so over a long engagement the fourth review in a row that would continue is left to you.

The limits are live in the terminal UI. An edit applies from the next turn, to engagements that are already running too. Lowering a limit below what an engagement has used ends it at its next check. Headless runs and `-p` read the limits once, at startup.

Most autopilot continuations stay inside the current run. So `[agent]` settings, MCP servers, and a profile that you change in the meantime apply when that run ends, as they do for a goal loop.

## See what it does

- **Transcript.** Every decision is a row with its reason, and so are the start and end of each engagement. Click a decision row to open what autopilot said, what happened next, which agent it was for, and what it cost. Delegate replies are marked as autopilot's, during the session and after a resume.
- **Cards.** A prompt, question, or plan card that the delegate is deciding says so, and does not take focus. When the delegate leaves it to you, the card says why, takes focus, and notifies you.
- **Footer.** A chip, `[autopilot · 12/100]` or `[ap]` in a narrow terminal, shows the continuations used against the limit. It turns amber while an item waits for you. Click it, or run `/autopilot` on an engaged session, to open the status panel. Its Status tab shows the goal and who owns it, budget use, the model, and waiting items. Its Log tab is [the log](#the-log). In the panel, `s` stops autopilot, `m` picks its model, and Esc closes it. See [Keybindings](/docs/keybindings/#context-specific).
- **Pane status.** While the delegate decides, the [pane status](/docs/notifications/#persistent-pane-status) reads as working. An item left to you reads as blocked.
- **Notifications.** An item left to you is an urgent notification, and the end of autopilot is a notice. Both follow [`ui.notifications`](/docs/notifications/#configuration).

### The log

The Log tab of the status panel lists what autopilot did in this session, newest first, under a header for each engagement. A header says when the engagement ran, how it ended, how many decisions it made, and what it cost. `/autopilot log` opens it at any time, so you can read an engagement after it ended.

Each entry records:

- what was decided, when, and in which engagement,
- what the delegate was asked, such as the permission request with its command, up to 2 KiB,
- what autopilot said, such as the guidance of a denial, the text of a reply, or the answers it picked,
- its reason,
- what happened next: the action applied, you answered first, you took the card, a reload or a rule settled it, the run stopped, or you answered after autopilot left it to you,
- the agent it was for, the model, the tokens, and the cost.

| Key | What it does |
|-----|--------------|
| `f` | Cycles the filter: all entries, the ones left to you or answered by you, then each kind of decision. |
| `y` | Copies the selected entry. |
| Enter | Jumps to the entry's row in the transcript. |
| Tab | Switches between Status and Log. |

Caudra keeps the last 2,000 decisions and 200 engagements of each session in its database, with the session. They go when the session is deleted, and a fork starts without them. Decisions made before preview 9 keep only their one-line record, and the panel shows them under Earlier decisions. Notifications never carry what the log holds. SDK clients read the same log with [`autopilot_log`](/docs/headless/#autopilot).

The delegate reads its recent decisions in the engagement, and sees where you answered instead of it, so it can follow your lead.

## Stop it or take over

You always win. Any key or click on a card the delegate is deciding makes the card yours, and the delegate stands down. On a question or a plan card, the key also does what it always does, F2 included. On a permission card, the first key only takes the card, so you read the request before you answer it.

These stop autopilot:

- `/autopilot off`, or `s` in the status panel,
- Esc Esc,
- Ctrl-C while a turn streams,
- Cancel on a question form,
- `interrupt` or `autopilot_stop` from an SDK client.

Esc Esc, Ctrl-C, Cancel, and `interrupt` also pause [automations](/docs/automations/), as they do without autopilot. `/reload`, `/cd`, and a profile switch keep autopilot engaged.

## Resume

A session that you resume stays engaged, with the budget it has used. When a goal is active, one goal check-in starts once the session settles, and it counts as a continuation. `/new` and a new tab start without autopilot.

## Safety

Autopilot is delegated consent. It approves calls you did not see. Read these limits before you leave a session alone:

- It never approves the reserved classes in the [permission table](#permission-prompts). Those need you, or a rule you wrote.
- It approves nothing while content screening has flagged the session. That flag needs an active [decision endpoint](/docs/providers/#decision-endpoints) and [`content_screening`](/docs/configuration/#decisions-features) turned on. Without them, autopilot still runs, and the [engage notice](#before-you-leave-it-alone) says so.
- It decides permission prompts without tool results, which keeps injected text one step away. A determined injection can still shape the assistant's own requests.
- It never adds a rule, so an approval does not outlive the call.
- It is no security boundary. Use [sandboxes](/docs/sandboxes/), [stored rules](/docs/permissions/#stored-rules), and read-only modes for what must never happen.

## Headless and CI

`--autopilot` works with `-p`. A `/goal <condition>` prompt then becomes the owned goal:

```sh
caudra -p --autopilot "/goal The test suite passes"
```

With no one present, an item the delegate leaves to you becomes a denial. The result reports how autopilot ended, and the exit code follows `is_error`, so a budget or error end fails the run and a stall that autopilot recovered from does not.

SDK clients engage with `autopilot_start`, stop with `autopilot_stop`, and read the state with `autopilot_status`. Autopilot events arrive as system messages. See [Headless Mode](/docs/headless/#autopilot) for the controls, the events, and the `autopilot` object in a `-p` result. ACP refuses `--autopilot`, because its client owns the permission prompts.

## Automations

[Automations](/docs/automations/) see autopilot, and they cannot engage it.

- `session.autopilot` is `()` while autopilot is off, and otherwise a map with `owns_goal` and `waiting`.
- `goal_finished` events carry `autopilot`, which is true when the goal was autopilot's.
- `needs_input` fires with kind `autopilot` when the delegate leaves an item to you. It is in the default inputs. While the delegate decides, nothing fires.
- `set_goal()` fails with `goal_owned_by_autopilot` while autopilot owns the goal, even with `replace: true`.
- Delegate answers and autopilot turns never count as your input, so they neither clear the automation pause nor reset the count for `max_unattended_turns`.

See [Automations](/docs/automations/#autopilot) for the details.

## The model and its cost

The delegate runs on the Autopilot [model job](/docs/providers/#model-jobs). Unbound, it follows Best, and the chat model is the last resort. Bind it with `/autopilot model` or in `/model`.

A bound model is never replaced. When it cannot be reached, or its answer is invalid three times, the delegate leaves the item to you with the reason.

Each decision is one model call without tools, with output capped at 4,096 tokens. The cost counts toward `max_cost`, and in the terminal UI it appears in [`/usage`](/docs/token-economy/) under the Autopilot purpose.
