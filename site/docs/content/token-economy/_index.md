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

**Deferred built-in tools.** Eight built-ins can start outside the request array behind that same `tool_search` entry. Caudra defers them for small and supply-unknown models, where the shorter array helps and the prompt cache is cheap to rebuild. Known non-small models receive them upfront because loading one mid-session can cost more than carrying all eight. See [Tools loaded on demand](/docs/tools/#which-models-defer).

**Managed tool output.** The host enforces `agent.max_output_bytes` and `agent.max_output_lines` after every tool dispatch. The same boundary covers Lua and MCP tools, batch children, nested calls, and local tools. `output_limits` can replace those defaults for one result. The host still performs limiting and retention after dispatch.

Successful text results larger than 8 KiB are retained. This lets Caudra prune older copies from [provider requests](/docs/context/#provider-request-projection) while preserving retrieval. When a result exceeds its effective configured limits, the model receives a bounded head and tail plus an opaque output ID instead of the complete text.

Use `tool_output_grep` with that ID and a regex to find relevant lines. Its `offset` is the first line to search, `limit` caps matches, and `context_before` and `context_after` add nearby lines. Then use `tool_output_read` with a 1-indexed `offset` and line `limit` to page through the needed range. Both tools include an exact next-call hint when more results remain. IDs belong to the current session. [Sessions](/docs/sessions/#managed-tool-outputs) covers retention and cleanup.

**Interrupted work is not wasted.** Press Esc on a long tool, or let its deadline hit, and whatever it printed so far still reaches the model, tagged as partial: `shell` keeps its streamed lines, `python_execution` the script output, a `task` subagent its half transcript. Otherwise the next turn starts from nothing and you pay to run it all again.

## Fewer round-trips

Every round-trip re-sends the context, so round-trips are the other half of the bill.

**batch** runs independent tool calls in one turn: one request, N results.

With `agent.eager_tool_dispatch = true` (the default), tools and batch children start as soon as their complete JSON arguments arrive. They still pass through permission checks, mode restrictions, and file locks. Later argument fragments cannot reset a running or completed child to queued. The old `agent.eager_batch_dispatch` setting remains a fallback when the new setting is absent.

An early call can apply effects before the provider finishes its response. A later argument revision cannot undo those effects. If the stream fails after calls were admitted, Caudra collects their outcomes and tells the model what happened instead of automatically replaying them. Explicit cancellation still stops running work.

**JSON syntax repair.** `agent.tool_json_repair = true` enables syntax repair independently of eager execution. Caudra first tries local repairs that preserve argument values. When local repair needs confirmation, one isolated request to the calling model receives only the affected tool schema, malformed arguments, and parser error. It receives no conversation history or execution tools.

Repairs retain the original call ID or batch-child slot. Valid siblings are not regenerated or executed again. Results record repaired arguments without rewriting the original assistant message. A schema error or execution failure, including a shell timeout, remains an ordinary tool error for the main model.

Repair accepts at most 64 KiB of complete argument text and refuses truncated values or ambiguous child boundaries. Model repair has a 20-second deadline, a 4,096-token output ceiling, at most two concurrent requests, and at most eight requests per response. Unsuccessful repairs return errors rather than guessed commands or file content.

Model repair adds usage charged under `tool_json_repair`. Its separate request leaves the main conversation prefix and tool catalog unchanged, avoiding an unnecessary prompt-cache invalidation. Cache hits still depend on the provider.

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

Some models charge more per token once a request's prompt passes a set size, and cached tokens count toward that size. Each request is priced by the size of its own prompt, so the long requests late in a session can cost more per token than the short ones at its start.

## Cache hit rate

Cached prompt tokens cost a fraction of fresh ones, so the share of your prompt that the provider served from cache is the clearest signal of whether context reuse is working.

`/usage` scores it in a `hit` column: `cache_read / (input + cache_creation + cache_read)`. The denominator is every prompt token the turn sent. Writing a cache counts against the rate, because those tokens were sent in full. The miss rate is the remainder, so a 92% hit means 8% missed. Output tokens are never cacheable and stay out of the arithmetic.

A rate of `—` means the provider reported no prompt tokens for that row, which is different from a hit rate of zero. Some providers report no cache counters at all, and their rows read as pure misses.

Both scopes of `/usage` score each model and, once two providers served the work, each provider. `caudra storage usage` prints the same column, and `--json` carries it as `cache_hit_rate`.

Some providers keep a cache per machine and route a request by a key the client supplies. Caudra sends one per conversation: the session id for the main agent, and `session/task` for a subagent, so siblings never compete for the parent's cache. Title, goal evaluator, requirements extraction, and tool-repair requests have their own system prompt and send no key. The key reaches OpenAI (as `prompt_cache_key`, plus the `session-id` header on a ChatGPT login), custom OpenAI-compatible endpoints, xAI, OpenRouter, Mistral, and a Claude login. It is a routing hint only, so a stale key costs a cache miss and never changes output.

Routing finds the right machine. Whether that machine holds a usable prefix is a separate matter. OpenAI writes a cache entry through the latest message of each request, so a conversation that shares the system prompt and tool definitions but opens with a different user turn, which is every new session and every subagent, finds no entry ending where its shared prefix ends. On GPT-5.6 and later Caudra places an explicit cache breakpoint after the system prompt, so that prefix is written once and read by every later conversation in the project. The mark is a field on an input block, and top-level `instructions` cannot carry it, so for these models the system prompt travels as the first developer message instead. A ChatGPT login does not take part: the Codex backend rejects the field, so a login keeps implicit caching, as do earlier models on either path.

Custom endpoints that speak the Responses protocol get the breakpoint when a model declares `supports_cache_breakpoints = true` in `providers.toml`. Leave it unset unless the server documents support, since a strict endpoint rejects the unknown field.

Cache writes on GPT-5.6 and later cost more than plain input, and `/usage` books them as cache creation so the hit column and the cost stay honest.

## Spend on a subscription

A Claude, ChatGPT, or Copilot login pays a flat monthly fee, so its turns never reach an invoice. Caudra still prices them at the provider's published API rates and files the figure separately, labelled `subscription (not billed)`. That is what the same work would have cost through the API.

The status bar has room for one number. It shows real spend when there is any, and puts a tilde in front when a subscription covered the session: `~$0.123`. Clicking the figure opens `/usage`, where the headline total stays money owed and the subscription figure sits on its own line beneath it.

Turns recorded before Caudra tracked the two apart are filed as billed spend, so an older ledger can overstate what you paid.

## Lifetime spend

Deleting a session deletes its transcript. The record of what it cost lives in a separate ledger that no session owns, so trimming and forgetting leave your spending history intact.

Project totals use the exact directory recorded for each turn. [Bulk session migration](/docs/sessions/#moving-sessions-to-another-directory) includes historical project usage by default, with an option to leave it unchanged. Moving one session preserves its own counters without reattributing the shared ledger.

Press `g` in `/usage` to switch from this session to everything ever recorded: totals, the providers, models, and projects that cost the most, and a month by month breakdown. Press `g` again to go back.

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
| `extract` | `/extract` lists and the requirements section of a compaction summary |
| `tool_json_repair` | Isolated syntax repair for malformed tool arguments |

The model cannot answer that question on its own, because goals, compaction, and titles often run on the model already in use.

Two things worth knowing about the numbers:

- Runs started with `--ephemeral` leave no session behind, and their spend is still recorded and labelled, so the totals stay complete.
- A model with no published price contributes tokens but no cost. Caudra reports how many turns those were rather than counting them as free, so the total is a floor.

The ledger holds one row per hour, model, project, and purpose, so it stays small on its own. [Retention](/docs/sessions/#retention) never touches it, and `--prune-older-than` is how you trim it.

## What the tools cost

Spend answers which models you paid for. `/tools` answers which tools filled the window they were paid for. It opens on the inventory, and `g` cycles through three recorded views before returning there.

| Scope | What it counts |
| --- | --- |
| Session | Calls made in the open transcript |
| Project | Calls made from this directory, across every session |
| Global | Every call Caudra has recorded |

Each view starts with a totals line and then one row per tool, ordered by call count.

| Column | Meaning |
| --- | --- |
| `Calls` | How many times the tool ran |
| `Err` | Share of those calls that failed |
| `Share` | The tool's share of every call in the scope |
| `Tokens` | Estimated tokens the results added to the context |
| `Tok%` | The tool's share of those tokens |
| `Time` | Wall clock spent inside the tool |
| `Time%` | The tool's share of tool time |
| `Avg` | Mean call |
| `p50` | The typical call, which the mean overstates once a few are slow |
| `p95` | The duration 95 calls in 100 finished within |

The table is wider than the modal, so a sideways scroll pans it.

Token figures carry a `~` because they are estimates. Caudra measures them with the o200k tokenizer, which is exact only for OpenAI models, and it measures the result the model actually received, after [result limits](#smaller-results) trimmed it. A tool with a small share of calls and a large share of tokens is the one to configure differently.

The percentiles are approximate for a different reason. A sum survives being merged across hours and projects while a percentile does not, so Caudra records the shape of each tool's durations as a small log-scale histogram rather than keeping every sample. Each figure names the top of the bucket a call landed in, which makes it an upper bound within roughly 12% of the real duration.

Tools that failed get a `Failures` block under the table naming the classes behind the rate: `cancelled`, `timed out`, `denied`, `not found`, `bad input`, and `failed` for the rest. A rate of 40% reads differently once you know it was `denied` every time.

Recorded activity lives in its own ledger, the same way spend does. Session counters go when the session goes. The project and global ledger holds one row per hour, tool, source, outcome, and project, so deleting a transcript leaves the record of what ran behind, and `caudra storage usage --prune-older-than` trims both ledgers in one pass.

Recording starts with this release. Earlier work is absent rather than counted as zero.
