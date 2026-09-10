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

The summary shows the active model and window size, estimated tokens grouped by source, the compaction reserve, and the space available before automatic compaction. `/context all` adds item-level built-in tool, MCP tool, profile, memory, and skill inventories. Opening either view does not add its report to the transcript.

`/tools` answers a narrower question: which tools the model can reach right now. It lists every built-in and MCP tool with its state, its token cost, and the rule behind that state. Tools turned off by configuration appear there and nowhere else, because they cost no context.

Every token count in the report is an estimate. Caudra uses local estimates for text and images. Provider tokenizers and wire formats vary, so the input total reported after a completed call can differ.

`/context` shows current capacity. `/usage` shows cumulative spend:

| Command | Scope | Numbers |
|---------|-------|---------|
| `/context` | One snapshot of the active Main or task window | Local estimates for the next projected request |
| `/usage` | Completed calls accumulated across the current session | Provider-reported tokens and priced spend, with a global view for lifetime spend |

Repeated requests increase `/usage` even when the current `/context` total stays flat. See [Token Economy](/docs/token-economy/#lifetime-spend) for the spending ledger.

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

Editing one of these files mid-session takes effect on your next message. The system prompt keeps the text it was built with, because rewriting it would invalidate the whole cached prefix on every save; the change reaches the model as a diff against that text instead. Compaction and `/undo` replace the conversation and have already given up that cache, so they quietly rebuild the system prompt from disk.

Putting a file back the way it was withdraws the diff rather than leaving it standing, as does a compaction that rebuilds the system prompt while a diff is outstanding. Either way the model is told to go back to following the system prompt as written.

Put coding conventions, repo quirks, and off-limits directories in these files. Keep them short; the next section explains why.

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

Caudra can replace old successful tool-result text with output-ID markers before sending a request to the provider. Only results retained for later retrieval are eligible. This reduces repeated context while keeping the result available through `tool_output_read` and `tool_output_grep`.

The `/context` report uses this provider projection rather than counting the raw transcript. Its message total can therefore be smaller than the on-disk log, and changing the active model or provider can change the projection. The replacement exists only in the provider request. Canonical session history stays intact. Compaction is separate and can rewrite the live log as described below.

## When the window fills

Long sessions eventually approach the model's context limit. Caudra reserves a slice of the window (`agent.compaction_buffer`) and before running out it summarizes everything that came before the most recent turns. Those recent turns carry over word for word, up to a quarter of the usable window and at most 15,000 tokens, so the work in flight keeps its exact detail. The cut always lands on one of your messages, which keeps every tool call paired with its result. When a session compacts more than once, the earlier summary is folded into the new one rather than summarized again. `/context` shows this compaction reserve separately from occupied context. The reserve is held capacity rather than content or spend. `/compact` triggers compaction early, and `agent.compaction_instructions` steers what the summary keeps.

The default reserve is 20%, because for most models the context window is the total the prompt and the response share, so the slice has to fit a whole reply. Where the window is an input budget instead and the output allowance sits on top of it, as with the wide Claude windows and the OpenAI Coding Plan models, the reserve only absorbs estimation drift and drops to 10%. Setting `agent.compaction_buffer` yourself overrides both.

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
