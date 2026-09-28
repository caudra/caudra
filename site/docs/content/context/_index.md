+++
title = "Context"
weight = 31
[extra]
group = "Concepts"
+++

# Context

Everything the model knows about your project passes through one context window, and every token in it costs money and attention. This page covers what Caudra puts there, when, and where you should put things so they land well.

## Inspect the active window

`/context` shows a compact snapshot of the context Caudra would send for the transcript currently open. Clicking the token counter in the status bar opens the same view. Main has one context. Each task has its own system prompt, tool set, and transcript, so opening a task and running `/context` reports that task alone. Return to Main to inspect Main. A task restored after restart has no request snapshot until the task is continued.

The summary shows the active model and window size, estimated tokens grouped by source, the compaction reserve, and the space available before automatic compaction. Once a call has been billed it also shows the size the provider measured, which is the figure the status bar draws and the one automatic compaction decides on. `/context all` adds item-level built-in tool, MCP tool, profile, memory, and skill inventories. Opening either view does not add its report to the transcript.

`/tools` answers a narrower question: which tools the model can reach right now. It lists every built-in and MCP tool with its state, its token cost, and the rule behind that state. Tools turned off by configuration appear there and nowhere else, because they cost no context. Press `g` to leave the inventory for what those tools have actually done, counted per session, per project, and across every run. See [Token Economy](/docs/token-economy/#what-the-tools-cost).

The per-source breakdown is an estimate. Caudra counts text and images locally with one tokenizer, which is exact only for the GPT-4o and GPT-5 families, so the total a provider bills for the same request can differ. The measured line and the status bar carry the provider's own count instead, extended by a local estimate of whatever arrived after it.

`/context` shows current capacity. `/usage` shows cumulative spend:

| Command | Scope | Numbers |
|---------|-------|---------|
| `/context` | One snapshot of the active Main or task window | Local estimates for the next projected request |
| `/usage` | Completed calls accumulated across the current session | Provider-reported tokens and priced spend, with a global view for lifetime spend |

Repeated requests increase `/usage` even when the current `/context` total stays flat. See [Token Economy](/docs/token-economy/#lifetime-spend) for the spending ledger.

`/projection` shows the request that `/context` counts: the system prompt, the tools, and the history as the provider receives them. It covers Main only. See [Inspect the projection](#inspect-the-projection).

## What loads when

```
session start (paid every request)   on demand (paid when used)
──────────────────────────────────   ─────────────────────────────────
effective system prompt              file contents   file_read / file_index / file_grep
tool definitions                     skill bodies    skill tool
instruction files (AGENTS.md, ...)   memory notes    memory tool
memory tag names                     subdir rules    first file_read there
skill names + descriptions           MCP tool defs   tool_search
```

The left column is the fixed overhead of every single request, so Caudra keeps it small on purpose. A selected [system prompt profile](/docs/system-prompts/) changes the effective system prompt and its overhead. A skill contributes one description line, memories one list of tags, and a big MCP server one search tool. The bodies stay on disk until the agent asks.

The `/context` views report what currently contributes without loading deferred material to size it. Large MCP installations contribute a compact `tool_search` catalog until the agent selects full definitions. Memory contributes its tag index and skills contribute their names and descriptions. A memory or skill body enters the transcript only when its tool reads it. These loads belong to the Main or task context that requested them.

## Mention a file with @

Typing `@` in the composer opens a path completion popup drawn from the same project walk as the file picker, so it honours your ignore rules and skips `.git`. Arrow keys move, Enter picks, Tab drills into a directory, Esc dismisses. You can also type the path yourself. The pointer works the same way: moving over a row marks it, the wheel walks the list, and a click takes the row under it, drilling into a directory the way `Tab` does.

A mention names a file, and optionally a line range:

| Written | Sent to the model |
|---------|-------------------|
| `@src/main.rs` | The whole file |
| `@src/main.rs:L42` | Line 42 |
| `@src/main.rs:L42-L88` | Lines 42 to 88 |
| `@src/` | A listing of the directory |
| `@"my notes.md"` | Quote a path that contains spaces |

At send time Caudra reads each mentioned file and puts the contents in the request ahead of your message, so the model has them without spending a turn on `file_read`. The reading goes through `file_read` itself, so truncation limits, binary detection and line numbering match what the model sees when it calls the tool. An image lands as a real image when the model supports vision. A file that is missing, binary or past the per-turn budget is reported to the model as a short note rather than silently omitted.

The transcript keeps the short `@src/main.rs:L42-L88` you typed. Contents are read once, when you send, so a later turn sees what the file said at that moment.

A mention only resolves when the path exists in your working directory. That is what keeps `@dataclass`, `@media` and an email address from being treated as files.

Caudra records a whole-file mention as a read, so a later edit is blocked if the file changed in between. A line range is not recorded, because seeing part of a file is not enough to edit the rest of it safely.

A mention marks itself as the pointer passes over it, the way a paste label does. Click it to open the file in the [workbench](/docs/workbench/), scrolled to the lines it names, with the explorer expanded to it and the row selected. `Ctrl+X Enter` in the workbench goes the other way, sending the file and the selected lines to the composer as a mention.

A mention you already sent stays clickable in the transcript. Hovering one puts the path in the status bar, the way a hovered link does, and clicking it opens the same workbench view. Only your own messages answer, so a path the model writes with an `@` is left as text. A mention wrapped in emphasis, a code span, or link text is left as text too: what reaches the screen there is no longer the path you typed.

## Mention a commit with #

Typing `#` opens a completion popup over the project's recent log. The fuzzy match runs across the hash, the subject, and the author together, so `#login` finds the commit that fixed login and `#ada` finds the ones Ada wrote. The keys and the pointer behave as they do in the file popup. Picking a row writes the abbreviated hash into your message.

The `#` is also what asks for the log. Caudra re-reads it each time the popup opens, so a commit the agent made during the session is there without restarting or changing directory. The read happens off the drawing thread, and the popup opens at once: on a spinner the first time, and on the window it already holds every time after, which it then replaces when the fresh one arrives.

At send time Caudra reads the commit and puts it in the request ahead of your message:

```xml
<commit hash="a1b2c3d" author="Ada Lovelace <ada@example.com>" date="2024-03-11T09:41:02Z">
<subject>Fix login crash on empty session</subject>
<message>
The guard read the session before the store had it...
</message>
<files>
M src/auth.rs
A src/guard.rs
</files>
</commit>
```

The file list carries the same letters git uses: `A` added, `D` deleted, `M` modified, `R` renamed, `C` copied, `T` type changed, `U` unmerged. Diff text is not included. Ask for the parts you want and the model will run `git show` itself, which keeps a large commit from filling the window.

A `#` resolves only when the hash names a commit in the loaded log window, and the check runs against that list rather than the repository. So a `#` in prose costs nothing, and `# Heading`, `#1234` and a CSS colour such as `#a1b2c3` stay text. The hash itself must be 7 to 40 hex characters. Symbolic revisions like `HEAD~3`, branch names, tags, and ranges are not mentions, because they mean different things on different days.

Before the first window arrives there is nothing to check against, so a hash spelled out to all 40 characters resolves on its own. Forty hex characters in a row is not something you type by accident, and send time can look it up for real. An abbreviation waits for the window. In a project with no repository at all, every `#` stays text.

Mentioned files and mentioned commits share one budget per turn, so a message carrying both cannot send more than a message carrying either. A commit past the budget, or one the log no longer holds, is reported to the model as a short note rather than dropped in silence.

A sent hash stays clickable in the transcript. Hovering one puts the commit subject in the status bar, which is more use than the hash you are already looking at, and clicking it opens [source control](/docs/workbench/) in the workbench with the cursor on that commit. Only your own messages answer, so a hash the model quotes back is left as text.

In a [remote workspace](/docs/remote-workspaces/) or a [sandbox](/docs/sandboxes/), `#` works the same way. The history lives on the far side, so the popup lists the log source control already read from the workspace rather than walking a repository on your machine, and the commit itself is fetched from the workspace when you send. That is one round trip, not two: the same read serves the popup and the source control pane.

## Instruction files

At session start Caudra walks from the project git root down to the working directory (no `.git` root, only the cwd). In each directory it loads **one** project instruction file, first match wins:

| Order | File |
|------|------|
| 1 | `AGENTS.md` |
| 2 | `CLAUDE.md` |
| 3 | `.github/copilot-instructions.md` |
| 4 | `COPILOT.md` |
| 5 | `.cursorrules` |
| 6 | `.windsurfrules` |
| 7 | `.clinerules` |
| 8 | `CONVENTIONS.md` |
| 9 | `GEMINI.md` |
| 10 | `CODING_AGENT.md` |

After the match it always loads `AGENTS.local.md` from the same directory if present: that one is yours, keep it gitignored. Closer directories win on conflicts. Finally one global `~/.config/caudra/AGENTS.md` for preferences that follow you across projects.

```
~/repo/AGENTS.md           loaded (root)
~/repo/AGENTS.local.md     loaded (yours, gitignored)
~/repo/api/CLAUDE.md       loaded when cwd is ~/repo/api, wins over root
~/repo/web/AGENTS.md       not loaded yet...
~/.config/caudra/AGENTS.md   loaded (global)
```

That `web/AGENTS.md` is not dead weight. The first time `file_read` opens a file under a subdirectory whose instruction file was never loaded, Caudra pulls it in. Monorepo rules live next to the code they govern and cost nothing until someone works there.

Each file arrives in the system prompt wrapped in a tag naming where it came from, so your rules cannot be mistaken for Caudra's own and a file that opens with a heading cannot read as a new prompt section:

```
<instructions scope="project" path="/home/you/repo/AGENTS.md">
...
</instructions>
```

`scope` is `project`, `local`, or `global`.

Editing one of these files mid-session takes effect on your next message. The system prompt keeps the text it was built with, because rewriting it would invalidate the whole cached prefix on every save; the change reaches the model as a diff against that text instead. A session that started with no instruction files has nothing in its prompt for a diff to patch, so the first file to appear arrives whole. Compaction and `/undo` replace the conversation and have already given up that cache, so they quietly rebuild the system prompt from disk.

Putting a file back the way it was withdraws the announcement rather than leaving it standing. Deleting a file that appeared does the same, and so does a compaction that rebuilds the system prompt while an announcement is outstanding. In each case the model is told to go back to following the system prompt as written.

Put coding conventions, repo quirks, and off-limits directories in these files. Keep them short, for the reason the next section gives.

### In a sandbox

A [remote workspace](/docs/remote-workspaces/) or [managed sandbox](/docs/sandboxes/) session has two filesystems. The workspace holds the project the model works on. Your own machine holds the checkout you launched Caudra from. Caudra reads instruction files from both.

The workspace goes first, walked with the same table and the same one-per-directory rule as above. Your machine goes second, walked the same way again. `~/.config/caudra/AGENTS.md` comes last, and it always comes from your machine, because a workspace has no copy of it.

Most files arrive twice, since the host checkout usually seeded the workspace. Caudra compares the text of each host file against every workspace file, ignoring trailing whitespace, and drops the ones that already arrived. The comparison is on content rather than path, because two filesystems have no reason to agree on where a file sits.

What survives is what the workspace does not have. `AGENTS.local.md` is the common case: keeping it gitignored is the recommendation above, and transfers respect gitignore, so it never reaches the workspace. A host file that has drifted from its workspace copy also survives, and you will see both versions.

Blocks carry one extra attribute in these sessions:

```
<instructions scope="local" origin="host" path="/home/you/repo/AGENTS.local.md">
...
</instructions>
```

`origin` is `workspace` for a file the workspace supplied and `host` for one read from your machine. Local sessions have a single filesystem, so they omit the attribute and their prompts stay byte-identical across upgrades.

Read the `host` blocks with that difference in mind. Your global file describes your machine, so a rule about an installed toolchain, an absolute path, or a local binary may not hold inside the workspace. The same caution applies to any host project file, especially when you started Caudra in a directory unrelated to the workspace you attached to. Nothing checks that the two are the same project.

A declared workspace file Caudra does not recognise is skipped, and the session warns you with its path. The rest of the project context still loads. The exception is `.caudra/permissions.toml`, which fails the session outright when it does not validate, since running without the restrictions a project asked for is worse than not running.

Paths the host cannot read are skipped the same way, whether a directory it cannot list or an instruction file it cannot open. The warning names each one. Files inside an unreadable directory are missing from the context, because the host never saw them. The permission file is the exception again. The session fails when the host cannot read `.caudra` or `.caudra/permissions.toml`, and also when it cannot list the workspace root.

## What Caudra writes on your behalf

Part of what the model reads was written by Caudra rather than typed by you. These messages carry your role, because that is the only role a provider accepts for them, and each one is wrapped in a `<system-reminder>` tag so the model can tell it apart from something you asked for.

| Injected | Sent |
|----------|------|
| Environment | At session start, and again whenever the date, working directory, or model changes |
| Mode announcement | On the first message after you switch between plan and build |
| Instruction change | On the first message after you edit or create an instruction file, and again to withdraw that when you put the file back |
| Goal check-in | While a goal is running, to report progress against it |
| Continuation | After a nudge or a compaction, to say what the model should pick up |
| Background work | Before an already-scheduled main request when task/workflow state changes or the configured response interval expires, and after successful compaction of a session with background work |
| Open todo items | When the main agent is about to hand control back while its todo list has pending or in-progress items, at most once per message you send |

These runtime snapshots arrive as messages, keeping the cached system prefix stable. Most are sent only when their content changes. Background-work snapshots can also repeat at a configured interval or after compaction.

Where one lands depends on whether anything else still holds a copy. A standing reminder — the environment, a mode announcement, an instruction change — is appended after the message it steers, and rewinding that message takes the reminder with it, because Caudra re-sends it on the next turn anyway. A one-shot notice — a finished background task, a settled workflow, the output of a `/!` command, an MCP prompt's canned exchange — is appended before the message, because it happened first and the transcript is the only place it still exists, so a rewind has to spare it.

Each one appears in the transcript as a dim row folded to its heading. Click the row to read the exact text the model was sent, and click again to fold it back. Mentioned file contents are the exception: the model gets them, and the transcript shows the `@path` you typed rather than the body behind it.

Set `ui.show_reminders = false` to keep the transcript to the conversation alone. The messages still reach the model.

### Background-work awareness

Caudra keeps the main agent aware of delegated work with a compact snapshot of background tasks, its own asynchronous shell jobs, and active workflows. It includes readable IDs, short assignments, current states, and counts for entries that do not fit. Child agents receive owner-scoped command state. Their shell jobs are not repeated as independent main-agent assignments. Snapshots read runtime state without polling tools or consuming result notifications.

Periodic refresh is disabled by default (`0`). To opt in, a conservative starting interval is 32 parent response groups:

```toml
[agent]
background_reminder_turns = 32
```

The interval counts parent response groups committed to history. A response with several tool calls counts once, as does a committed partial or reasoning-only response. Child responses, compaction summaries, and retries without a committed response do not advance the count. Set `0` to disable periodic refresh. State-change and successful post-compaction refresh remain enabled.

Reminders accompany requests the main agent was already going to make. They do not wake an idle chat, poll on a timer, or override Stop. Actual task reports and outcomes retain their [automatic continuation behavior](/docs/sessions/#background-tasks).

Compaction preserves a Delegated work section with assignments, scope, expected results, and the parent's next steps. After summarization, Caudra reads runtime state again so a child that finished during compaction is not described as still running. Summary status is last observed. The host snapshot supplies current execution state, while attributed reports supply results. When nothing remains active, a clearing snapshot does not claim that the user's overall request is complete.

### Open todo items

When the main agent ends its answer while its todo list still has pending or in-progress items, Caudra holds the handoff once and sends a reminder. The reminder repeats the whole list as last recorded, including completed and cancelled items. It asks the model to check each open item against the work, correct the list with `todo_write`, and continue any work it can do now. Blocked items, items waiting on you, and background work may stay open. After one more answer, control returns to you whatever the list says.

The reminder adds at most one request per message you send. Subagents never receive it. Runs that are cancelled, fail, or reach the turn limit end as they would without it. A resumed session reads its last list back from the stored transcript, including across compaction.

To turn it off:

```toml
[agent]
todo_reminder = false
```

## Four places to put knowledge

All four end up in context, but at different times and prices:

| | Loaded | Costs | Good for |
|---|--------|-------|----------|
| `AGENTS.md` | every session | every request | short rules: conventions, build commands, no-go areas |
| [Skills](/docs/skills/) | when the agent picks one | a description line until then | long playbooks: release process, plugin authoring |
| Memory | when the agent recalls a tag | tag names until then | gotchas the agent learns while working |
| [Commands](/docs/commands/) | when you type `/name` | nothing until invoked | prompts you keep retyping |

Rule of thumb: when `AGENTS.md` grows past a screen, the new material probably wants to be a skill. `AGENTS.md` is a tax on every request; a skill is a tax only on the sessions that need it.

## Provider request projection

The request for each turn carries a projection of the session history: a copy adapted to the model and provider the request goes to. Five steps build it.

1. **Old-result pruning.** A successful tool result that Caudra [retained](/docs/token-economy/#smaller-results), from before your last two messages, becomes a marker naming its output ID, so the model can still read or search the full result with `tool_output`. The newest of those results stay whole until the next one would take them past 40,000 tokens, and pruning starts only once the rest add up to more than 20,000. Results from `tool_output` and `skill` are never pruned. The step needs `tool_output` in the request's tools, because the marker points there.
2. **Foreign-reasoning lowering.** Reasoning goes back as reasoning only to the provider, model, and API that produced it, and only when it is complete. Otherwise it is sent as plain text, and redacted reasoning and signatures are dropped, because only the provider that issued them can use them.
3. **Empty-turn filler.** An assistant turn with no text and no tool call, such as one that only reasoned, is sent with the text `(empty)`, since providers reject an empty turn. When a model stalls on empty replies, only the latest empty turn and the recovery prompt after it are sent, so the model does not take a run of them as the pattern to follow.
4. **Tool-pair repair.** Providers reject a tool call without its result and a result without its call. A result whose call is missing is dropped, and a call whose result is missing is answered with the error `[Tool result not available]`.
5. **Image fallback.** For a model without image input, each image becomes a note saying it was omitted. Switching back to a vision model sends the images again.

Each change exists only in the request. The stored session history stays intact, so the next request is projected from the original again. Compaction is separate and can rewrite the live log as described below.

The live request, `/context`, and `/btw` all use this projection. `/context` counts it rather than the raw transcript, so its message total can be smaller than the on-disk log, and changing the active model or provider can change the projection.

### Inspect the projection

`/projection` shows the conversation as the provider receives it: the system prompt, the tools, and the history after these five steps. It covers Main only. The modal takes a snapshot when it opens and holds still while you read, so reopen it to see a newer request.

The Projection view lays the request out in sections. `SYSTEM` holds the system prompt. `TOOLS` holds each tool offered in full: its name, its description as written, and its input schema and other fields as JSON. Each message follows as `#1 USER`, `#2 ASSISTANT`, and so on, with its text as written and its markdown syntax visible. Labels mark each `thinking`, `redacted thinking`, `tool_use`, `tool_result`, and `image` block, and an image shows its media type and size in place of the pixels. Dim tags on a message header, such as `synthetic` or `compaction summary`, record what Caudra knows about the message and never sends.

`n` and `p` jump to the next and previous section. Drag to select a passage, and releasing the mouse button copies it. Press `Ctrl+A` to select the whole view. With a selection standing, `y` and `Ctrl+C` copy it. With nothing selected, `y` copies the whole view as unwrapped text without the bars in the margin, and `Ctrl+C` closes the modal.

Press `r` for the Wire view, the JSON body the active provider would send for the same messages, and `r` again to go back. The title shows the method and URL. Headers are left out, and credentials travel only in headers, so no key or token appears. Caudra builds the body with the same code as the real request, using only the credentials and caches it already holds. When those are not enough yet, the view says why: Copilot, for example, learns its API endpoint from the first message you send.

The body is pretty-printed for reading rather than byte-identical to the one sent. Long lines run past the edge rather than wrapping, and `Shift+Left` and `Shift+Right` pan across them as in any [wide modal](/docs/keybindings/#focus). A long base64 string, such as image data or encrypted reasoning, is shortened on screen to its size, and `y` copies the body with every string whole.

Opened while tool calls are still running, both views leave the last assistant turn's calls open, without the placeholder results that repair would add. The Wire body then carries those calls with no results. Each view ends with a note that their results join the next request.

A view taller than 65,535 rows keeps its newest rows behind a notice that the earlier ones are left out. `Ctrl+A` then selects only the rows kept. With nothing selected, `y` still copies everything.

## When the window fills

Long sessions eventually approach the model's context limit. Caudra reserves a slice of the window (`agent.compaction_buffer`) and before running out it summarizes everything that came before the most recent turns. Those recent turns carry over word for word, up to a quarter of the usable window and at most 15,000 tokens, so the work in flight keeps its exact detail. The cut always lands on one of your messages, which keeps every tool call paired with its result. When a session compacts more than once, the earlier summary is folded into the new one rather than summarized again. `/context` shows this compaction reserve separately from occupied context. The reserve is held capacity rather than content or spend. `/compact` triggers compaction early, and `agent.compaction_instructions` steers what the summary keeps.

A main-session summary ends with a `# User requirements` section: the list of what you asked for, drawn by the Extract model from every message you wrote, every question you answered, and the reply each message was answering, across the whole transcript rather than only the turns being summarized. It runs beside the summary rather than inside it, so it costs no extra wait, and a failed extraction carries the previous summary's section forward. `/extract` shows the same list on demand, and `agent.compaction_requirements = false` turns the section off. See [Requirements](/docs/commands/#requirements).

The default reserve is 20%, because for most models the context window is the total the prompt and the response share, so the slice has to fit a whole reply. Where the window is an input budget instead and the output allowance sits on top of it, as with the wide Claude windows and the OpenAI Coding Plan models, the reserve only absorbs estimation drift and drops to 10%. Setting `agent.compaction_buffer` yourself overrides both.

The reserve moves the point where compaction fires below the window, so the status bar counter names both: `300.1k/372k (81%/90%)` reads as 81% of the window in use and compaction at 90%. Exact multiples omit the empty decimal. When its row has room, a gauge in front of the counter draws the same pair as `▕████████░│▏`, filled to the share in use with a tick in the cell where compaction fires. Both turn amber once the border is behind you. A narrow terminal drops the gauge first, then the border, then the counts, then the figure itself. `/context` names the same pair on its `Used` and `Measured` lines, showing tenths when they are nonzero.

Compaction replaces the summarized turns in the session's on-disk log with the summary and keeps the preserved ones after it. The dropped turns are not lost: before the rewrite, Caudra parks the previous log at `sessions/archive/<session-id>/<n>.jsonl` in the [state directory](/docs/configuration/#directory-layout). It keeps the newest three per session, and at most 32 MB of them. The names count up, so the highest number is the newest.

An archive is a complete session file, so `jq` or an editor reads it as it is. To open one in Caudra you have to put it back in place of the live log, which drops the session's current state, so move that out of the way first:

```sh
cd ~/.local/state/caudra/sessions
mv <session-id>.jsonl <session-id>.jsonl.bak
cp archive/<session-id>/<n>.jsonl <session-id>.jsonl
caudra -s <session-id>
```

`CAUDRA_DISABLE_AUTOCOMPACT=1` turns off the automatic compaction. A manual `/compact` still compacts.

Related: [Token Economy](/docs/token-economy/) for why all this frugality exists, [Configuration](/docs/configuration/) for the knobs.
