+++
title = "Token Economy"
weight = 30
[extra]
group = "Concepts"
+++

# Token Economy

Caudra's whole design falls out of one fact about agent loops: the conversation is re-sent to the model on every turn.

```
turn 1   [system + prompt]                      ─► model ─► tool call
turn 2   [system + prompt + result 1]           ─► model ─► tool call
turn 3   [system + prompt + result 1 + 2]       ─► model ─► ...
```

A tool result does not cost its tokens once. It costs them again on every turn until the session ends or history is compacted. `cat` a 2000-line file on turn 2 of a 40-turn session and you pay for it 38 more times. Prompt caching softens the price, not the principle: cache reads still cost, and a bloated context also makes models measurably dumber.

So Caudra attacks the two multipliers: how much each step adds to context, and how many steps there are.

## Smaller results

**file_index instead of file_read.** The native `file_index` tool returns a tree-sitter skeleton of a source file: imports, types, signatures, line numbers. Usually 70-90% smaller than the file itself. The agent indexes first, then reads only the ranges it needs.

Directory indexing follows Workcell's generic listing contract. Instruction files appear as ordinary visible entries, and the call does not discover their contents.

```
file_read main.rs            index main.rs
─────────────────            ─────────────────────────────
1400 lines in context        60 lines of signatures
                             + file_read offset=812 limit=40
```

**Subagents as garbage collectors.** A `task` subagent gets its own isolated context. It can search, read files, and hit dead ends as much as it wants while only its final summary enters the main conversation. Its transcript stays attached to the task for later `task_id` continuation without inflating the main context. System prompt profiles can assign a different model to their subagents when a task needs a cheaper or stronger model.

```
main context                subagent context (isolated)
────────────                ────────────────────────────
task("find auth") ───────►  file_glob, file_grep ×6, file_read ×9, ...
                  ◄───────  "JWT middleware, auth.rs:120"
one line stays              ~20k tokens stay outside main
```

**Deferred MCP tools.** An MCP server with 100 tools would ship 100 definitions in every request. Caudra loads a single `tool_search` tool instead; the model searches when it actually needs something and only the matches load. See [MCP](/docs/mcp/#tool-search).

**Managed tool output.** The host enforces `agent.max_output_bytes` and `agent.max_output_lines` after every tool dispatch. The same boundary covers Lua and MCP tools, batch children, nested calls, and local tools. `output_limits` can replace those defaults for one result. The host still performs limiting and retention after dispatch.

Successful text results larger than 8 KiB are retained. This lets Caudra prune older copies from [provider requests](/docs/context/#provider-request-projection) while preserving retrieval. When a result exceeds its effective configured limits, the model receives a bounded head and tail plus an opaque output ID instead of the complete text.

Use `tool_output_grep` with that ID and a regex to find relevant lines. Its `offset` is the first line to search, `limit` caps matches, and `context_before` and `context_after` add nearby lines. Then use `tool_output_read` with a 1-indexed `offset` and line `limit` to page through the needed range. Both tools include an exact next-call hint when more results remain. IDs belong to the current session. [Sessions](/docs/sessions/#managed-tool-outputs) covers retention and cleanup.

**Interrupted work is not wasted.** Press Esc on a long tool, or let its deadline hit, and whatever it printed so far still reaches the model, tagged as partial: `shell` keeps its streamed lines, `python_execution` the script output, a `task` subagent its half transcript. Otherwise the next turn starts from nothing and you pay to run it all again.

## Fewer round-trips

Every round-trip re-sends the context, so round-trips are the other half of the bill.

**batch** runs independent tool calls in one turn: one request, N results.

**python_execution** runs pure computation in an isolated Python subset. It can reshape JSON, aggregate values, process text, and perform calculations without host filesystem or network access.

```
manual calculation              with python_execution
─────────────────────            ─────────────────────────────
inspect a large JSON result      data = json.loads(source)
reason over every value          print(sum(row["cost"] for row in data))
more context and mistakes        one bounded result
```

**Compaction** resets the multiplier when a session runs long: older turns are summarized and dropped. [Context](/docs/context/#when-the-window-fills) has the details.

## Watching it work

`/usage` shows the token breakdown of the current session, and `--output-format json` in [Headless Mode](/docs/headless/) reports `total_cost_usd` per run. Cheap is a feature you can measure.

Each turn is priced when it happens and that number is stored with the session. Prices move (DeepSeek, for one, doubles every rate during peak UTC hours), so a total re-priced later would be a guess. What you see is what you were billed.

## Spend on a subscription

A Claude, ChatGPT, or Copilot login pays a flat monthly fee, so its turns never reach an invoice. Caudra still prices them at the provider's published API rates and files the figure separately, labelled `subscription (not billed)`. That is what the same work would have cost through the API.

The status bar has room for one number. It shows real spend when there is any, and puts a tilde in front when a subscription covered the session: `~$0.123`. In `/usage` the headline total stays money owed, and the subscription figure sits on its own line beneath it.

Turns recorded before Caudra tracked the two apart are filed as billed spend, so an older ledger can overstate what you paid.

## Lifetime spend

Deleting a session deletes its transcript. The record of what it cost lives in a separate ledger that no session owns, so trimming and forgetting leave your spending history intact.

Press `g` in `/usage` to switch from this session to everything ever recorded: totals, the models and projects that cost the most, and a month by month breakdown. Press `g` again to go back.

From the shell:

```bash
caudra storage usage                          # by model, all time
caudra storage usage --group-by project       # where the money went
caudra storage usage --group-by purpose       # chat against everything else
caudra storage usage --group-by month --json  # for a spreadsheet
caudra storage usage --since 30d
caudra storage usage --prune-older-than 1y
```

Every row records why the model was called, so you can separate the conversation from the work Caudra does around it:

| Purpose | What it covers |
| --- | --- |
| `chat` | Conversation turns, including the ones subagents run |
| `goal` | [Completion goal](/docs/commands/#completion-goals) evaluations |
| `compaction` | Summarizing a session that filled its window |
| `title` | Naming a session |
| `btw` | `/btw` questions asked beside the conversation |

The model cannot answer that question on its own, because goals, compaction, and titles often run on the model already in use.

Two things worth knowing about the numbers:

- Runs started with `--ephemeral` leave no session behind, and their spend is still recorded and labelled, so the totals stay complete.
- A model with no published price contributes tokens but no cost. Caudra reports how many turns those were rather than counting them as free, so the total is a floor.

The ledger holds one row per hour, model, project, and purpose, so it stays small on its own. [Retention](/docs/sessions/#retention) never touches it, and `--prune-older-than` is how you trim it.
