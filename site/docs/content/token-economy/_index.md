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

**index instead of file_read.** The native `index` tool returns a tree-sitter skeleton of a source file: imports, types, signatures, line numbers. Usually 70-90% smaller than the file itself. The agent indexes first, then reads only the ranges it needs.

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

**Interrupted work is not wasted.** Press Esc on a long tool, or let its deadline hit, and whatever it printed so far still reaches the model, tagged as partial: `shell` keeps its streamed lines, `code_execution` the script output, a `task` subagent its half transcript. Otherwise the next turn starts from nothing and you pay to run it all again.

## Fewer round-trips

Every round-trip re-sends the context, so round-trips are the other half of the bill.

**batch** runs independent tool calls in one turn: one request, N results.

**code_execution** runs pure computation in an isolated Python subset. It can reshape JSON, aggregate values, process text, and perform calculations without host filesystem or network access.

```
manual calculation              with code_execution
─────────────────────            ─────────────────────────────
inspect a large JSON result      data = json.loads(source)
reason over every value          print(sum(row["cost"] for row in data))
more context and mistakes        one bounded result
```

**Compaction** resets the multiplier when a session runs long: older turns are summarized and dropped. [Context](/docs/context/#when-the-window-fills) has the details.

## Watching it work

`/usage` shows the token breakdown of the current session, and `--output-format json` in [Headless Mode](/docs/headless/) reports `total_cost_usd` per run. Cheap is a feature you can measure.

Each turn is priced when it happens and that number is stored with the session. Prices move (DeepSeek, for one, doubles every rate during peak UTC hours), so a total re-priced later would be a guess. What you see is what you were billed.
