---
title: "Token Economy"
description: "Where tokens go in an agent loop, and every trick Caudra uses to spend fewer of them."
---

Caudra's whole design falls out of one fact about agent loops: the conversation is re-sent to the model on every turn.

```
turn 1   [system + prompt]                      ─► model ─► tool call
turn 2   [system + prompt + result 1]           ─► model ─► tool call
turn 3   [system + prompt + result 1 + 2]       ─► model ─► ...
```

A tool result does not cost its tokens once. It costs them again on every turn until the session ends or history is compacted. `cat` a 2000-line file on turn 2 of a 40-turn session and you pay for it 38 more times. Prompt caching softens the price, not the principle: cache reads still cost, and a bloated context also makes models measurably dumber.

So Caudra attacks the two multipliers: how much each step adds to context, and how many steps there are.

## Smaller results

**Built-in shell output filtering.** Workcell provides RTK-style output reduction without a separate RTK installation. Built-in rules reduce completed model-facing shell output while keeping the reviewed command and structured capture unchanged. The TUI shows raw output while the command runs, then lets you toggle between filtered and raw views. Filtering is enabled by default. Set `agent.shell_output_filter = false` or use `--no-rtk` to disable it. See [shell output](/docs/tools/#shell). Capture and retention bounds still apply.

A rule that would replace output with a success summary applies only when the command exited zero, so a failure never reads as a success. When a failing command reaches a rule's line cap, the result keeps its first and last lines, and an error printed at the end still reaches the model. When filtering would save less than its own notice costs, the complete capture is returned instead, so a filtered result is always smaller than the raw output. A filtered result carries a `[filtered: …]` line that names the stages which changed it. Most rules come from [RTK](https://github.com/rtk-ai/rtk), and Workcell credits it in its notice file.

**Progress bars collapse to one row.** A download or training run that redraws its bar with carriage returns writes a single line that can run to hundreds of kilobytes. Workcell decodes that stream into the rows a terminal would show and adds a line counting the redraws it absorbed. This decoding applies to every command, even with filtering disabled. Progress printed one frame per line, as a CI log collector writes it, is collapsed by filtering and announced as `[filtered: progress]`.

**file_index instead of file_read.** The native `file_index` tool returns a tree-sitter skeleton of a source file: imports, types, signatures, line numbers. Usually much smaller than the file itself. The agent indexes first, then reads only the ranges it needs.

Directory indexing follows Workcell's generic listing contract. Instruction files appear as ordinary visible entries, and the call does not discover their contents.

```
file_read main.rs            file_index main.rs
─────────────────            ─────────────────────────────
1400 lines in context        60 lines of signatures
                             + file_read offset=812 limit=40
```

**Source-code graph instead of exploring.** `source_code_context` finds the code a task touches, and `source_code_refs` lists who calls a symbol and from which lines. With `depth`, `source_code_refs` also shows what a change could reach and which tests cover it. One call replaces a round of grep and whole-file reads. They build their symbol graph from the source when called, and a repeat call parses only the files that changed. There is no index to build or maintain, and they work without configuration. See [Code Intelligence](/docs/tools/#code-intelligence).

**Subagents as garbage collectors.** A `task` subagent gets its own isolated context. It can search, read files, and hit dead ends without adding its whole transcript to the main conversation. The parent receives its final summary and any intentional [child reports](/docs/sessions/#child-reports). Its transcript stays attached to the task for later `task_id` continuation without inflating the main context. System prompt profiles can assign a different model to their subagents when a task needs a cheaper or stronger model.

```
main context                subagent context (isolated)
────────────                ────────────────────────────
task("find auth") ───────►  file_glob, file_grep ×6, file_read ×9, ...
                  ◄───────  "JWT middleware, auth.rs:120"
one line stays              ~20k tokens stay outside main
```

**MCP code mode.** An MCP server with 100 tools would ship 100 definitions in every request. Caudra sends none of them, and the model calls your servers from Python in the one fixed [`code`](/docs/code/) tool. It learns their tools from a compact server list, searches for the rest, and filters results in the snippet, so only what it keeps reaches the context. A server that connects or changes its tools never touches the cached prefix. See [Code mode](/docs/mcp/#code-mode).

**Tools as `code` modules.** Image generation, the host description, workflows, and automations are [modules of the `code` tool](/docs/code/#modules) rather than tools of their own. The `code` description carries one line for each, and the model loads a module's full reference with `help()` only when it needs it. The request array stays short for every model and never changes mid-session, so the prompt cache holds.

**Managed tool output.** The host enforces `agent.max_output_bytes` and `agent.max_output_lines` after every tool dispatch. The same boundary covers Lua and MCP tools, batch children, nested calls, and local tools. `output_limits` can replace those defaults for one result. The host still performs limiting and retention after dispatch.

Successful text results larger than 8 KiB are retained. This lets Caudra prune older copies from [provider requests](/docs/context/#provider-request-projection) while preserving retrieval. When a result exceeds its effective configured limits, the model receives a bounded head and tail plus an opaque output ID instead of the complete text.

Use [`tool_output`](/docs/tools/#tool_output) with that ID as `output_id` and a regex `pattern` to find relevant lines. Its `offset` is the first line to search, `limit` caps matches, and `context_before` and `context_after` add nearby lines. Omit `pattern` to page through the needed range with a 1-indexed `offset` and line `limit`. The tool includes an exact next-call hint when more results remain. IDs belong to the current session. [Sessions](/docs/sessions/#managed-tool-outputs) covers retention and cleanup.

**Interrupted work is not wasted.** Press Esc on a long tool, or let its deadline hit, and whatever it printed so far still reaches the model, tagged as partial: `shell` keeps its streamed lines, a `task` subagent its half transcript. Otherwise the next turn starts from nothing and you pay to run it all again.

## Fewer round-trips

Every round-trip re-sends the context, so round-trips are the other half of the bill.

**batch** runs independent tool calls in one turn: one request, N results.

With `agent.eager_tool_dispatch = true` (the default), tools and batch children start as soon as their complete JSON arguments arrive. They still pass through permission checks, mode restrictions, and file locks. Later argument fragments cannot reset a running or completed child to queued. The old `agent.eager_batch_dispatch` setting remains a fallback when the new setting is absent.

An early call can apply effects before the provider finishes its response. A later argument revision cannot undo those effects. If the stream fails after calls were admitted, Caudra collects their outcomes and tells the model what happened instead of automatically replaying them. Explicit cancellation still stops running work.

The transcript draws each call while the model writes it. A shell command, file contents, Python code, a plan, a memory note, an image prompt, or a subagent brief fills in before the call runs. A `file_edit` shows its path and a growing size until it runs, and a patch names the files it touches.

**JSON syntax repair.** `agent.tool_json_repair = true` enables syntax repair independently of eager execution. Caudra first tries local repairs that preserve argument values. When local repair needs confirmation, one isolated request to the calling model receives only the affected tool schema, malformed arguments, and parser error. It receives no conversation history or execution tools.

Repairs retain the original call ID or batch-child slot. Valid siblings are not regenerated or executed again. Results record repaired arguments without rewriting the original assistant message. A schema error or execution failure, including a shell timeout, remains an ordinary tool error for the main model.

Repair accepts at most 64 KiB of complete argument text and refuses truncated values or ambiguous child boundaries. Model repair has a 20-second deadline, a 4,096-token output ceiling, at most two concurrent requests, and at most eight requests per response. Unsuccessful repairs return errors rather than guessed commands or file content.

Model repair adds usage charged under `tool_json_repair`. Its separate request leaves the main conversation prefix and tool catalog unchanged, avoiding an unnecessary prompt-cache invalidation. Cache hits still depend on the provider.

**code** runs pure computation in a Python subset, in a worker process with no host filesystem or network access. It can reshape JSON, aggregate values, process text, and perform calculations, and its variables carry over to the next run, so a large result is loaded once and questioned many times. It also [searches past conversations](/docs/code/#search-past-conversations), so the model finds an earlier decision with one call instead of re-reading old sessions.

```
manual calculation              with code
─────────────────────            ─────────────────────────────
inspect a large JSON result      data = json.loads(source)
reason over every value          print(sum(row["cost"] for row in data))
more context and mistakes        one bounded result
```

**Compaction** resets the multiplier when a session runs long: older turns are summarized and dropped. [Context](/docs/context/#when-the-window-fills) has the details.

## Watching it work

`/usage` shows the token breakdown of the current session, and `--output-format json` in [Headless Mode](/docs/headless/) reports `total_cost_usd` per run. Cheap is a feature you can measure.

A tool card that loads text into the context says about how many tokens that text costs, after what it found: `209 lines · ~2.1k tokens` on a file read or a skill, `exit 0 · ~840 tokens` on a command, `12 matches in 3 files · ~610 tokens` on a search. File reads and searches, file indexes, commands, `code` runs, skills, subagent answers, source-code lookups, web pages and MCP results all say it. An instruction file a read pulls in, such as an `AGENTS.md`, gets a row of its own with its own count. The count is an estimate of what the model reads. Inside a `code` card, the model reads only what the snippet printed or returned, so the `code` card's own count is what the run cost, whatever a call's row inside it says.

Caudra counts these tokens locally with the o200k tokenizer, which is exact only for OpenAI models. To get closer on other models, it learns how far each model's own tokenizer is from that count. Between two requests of a tool loop, the provider bills the turn and the results that were added, and Caudra compares that bill with its own count of the same text. A larger result counts for more, an older comparison counts for half after about 20 newer ones, and the learned ratio stays between 0.5 and 2. A request that added a new prompt, an image, or a document, or that changed the system prompt or the tools, teaches it nothing. What it learned is kept in the [state directory](/docs/configuration/#directory-layout) and used for every count on that model, from the cards to [`/context`](/docs/context/) and the tool ledger. A card keeps the count it had when its result arrived, and a reopened session counts with what is known now.

Each turn is priced when it happens and that number is stored with the session. Prices move (DeepSeek, for one, doubles every rate during peak UTC hours), so a total re-priced later would be a guess. What you see is what you were billed.

Some models charge more per token once a request's prompt passes a set size, and cached tokens count toward that size. Each request is priced by the size of its own prompt, so the long requests late in a session can cost more per token than the short ones at its start.

## Cache hit rate

Cached prompt tokens cost a fraction of fresh ones, so the share of your prompt that the provider served from cache is the clearest signal of whether context reuse is working.

`/usage` scores it in a `hit` column: `cache_read / (input + cache_creation + cache_read)`. The denominator is every prompt token the turn sent. Writing a cache counts against the rate, because those tokens were sent in full. The miss rate is the remainder, so a 92% hit means 8% missed. Output tokens are never cacheable and stay out of the arithmetic.

A rate of `—` means the provider reported no prompt tokens for that row, which is different from a hit rate of zero. Some providers report no cache counters at all, and their rows read as pure misses.

Both scopes of `/usage` score each model and, once two providers served the work, each provider. `caudra storage usage` prints the same column, and `--json` carries it as `cache_hit_rate`.

A hit needs the request to begin with the same bytes as an earlier one, so Caudra keeps the tool definitions and the system prompt fixed while a conversation runs. It rebuilds them only at the moments listed in [When Caudra rebuilds](/docs/configuration/#when-caudra-rebuilds). Config edits, edits to [instruction files](/docs/context/#instruction-files), and notes other sessions add to [memory](/docs/memory/#when-the-view-changes) reach the model as reminder messages in between.

Some providers keep a cache per machine and route a request by a key the client supplies. Caudra sends one per conversation: the session id for the main agent, and `session/task` for a subagent, so siblings never compete for the parent's cache. Title, goal evaluator, requirements extraction, and tool-repair requests have their own system prompt and send no key. The key reaches OpenAI (as `prompt_cache_key`, plus the `session-id` header on a ChatGPT login), custom OpenAI-compatible endpoints, xAI, OpenRouter, Mistral, and a Claude login. It is a routing hint only, so a stale key costs a cache miss and never changes output.

A cache belongs to one account. When a provider has [several accounts](/docs/accounts/), Caudra keeps each conversation on its account and moves it at a point where its cache is cold anyway, such as a new session or a compaction. It moves a warm conversation only when its account is nearly spent or refused, and the transcript row for that switch says how many cached tokens the new account sends again.

Routing finds the right machine. Whether that machine holds a usable prefix is a separate matter. OpenAI writes a cache entry through the latest message of each request, so a conversation that shares the system prompt and tool definitions but opens with a different user turn, which is every new session and every subagent, finds no entry ending where its shared prefix ends. On GPT-5.6 and later Caudra places an explicit cache breakpoint after the system prompt, so that prefix is written once and read by every later conversation in the project. The mark is a field on an input block, and top-level `instructions` cannot carry it, so for these models the system prompt travels as the first developer message instead. A ChatGPT login does not take part: the Codex backend rejects the field, so a login keeps implicit caching, as do earlier models on either path.

Custom endpoints that speak the Responses protocol get the breakpoint when a model declares `supports_cache_breakpoints = true` in `providers.toml`. Leave it unset unless the server documents support, since a strict endpoint rejects the unknown field.

Cache writes on GPT-5.6 and later cost more than plain input, and `/usage` books them as cache creation so the hit column and the cost stay honest.

## Cache warming

Anthropic keeps a prompt cache entry for 5 minutes after the request that last read or wrote it. A shell command, a subagent, or a question that takes longer lets the entry expire, and the next request writes the whole prompt again at the cache-write price. On a long conversation that one write can cost more than a dozen reads.

Cache warming sends the last request again just before the entry expires, with `max_tokens` set to 0. The model generates nothing, the provider bills the prompt as a cache read, and the entry lives another 5 minutes. It is off by default. Turn it on in the global `caudra.toml`:

```toml
[agent]
cache_warming = "running"
```

| Mode | Warms |
| --- | --- |
| `off` | Never. This is the default. |
| `running` | While a run waits on a tool, a subagent, a question, or a permission prompt. |
| `idle` | Also for up to 30 minutes after a run ends, so a reply after a short break still reads the cache. |

A project `caudra.toml` cannot set it, because it spends your quota. In the terminal UI an edit to it applies as soon as you save, so turning it off also stops warming already under way; a subagent already running keeps its mode until it ends. Headless runs and ACP read it at startup. Subagents warm their own conversations, and in `idle` mode they stop when their run ends.

Warming covers requests sent straight to the Anthropic API. Bedrock, Vertex, other gateways, and a custom base URL never warm. A request also never warms when it has a fixed thinking budget, structured output, or a forced tool choice, because the provider either refuses `max_tokens: 0` for it or caches it under a different key.

Caudra warms only when it is likely to pay off:

- The prompt has at least 4,096 tokens, and one avoided cache write is worth at least $0.05 more than a warm.
- Each warm goes to the account that served the request, never to another one, and never refreshes a login. An account that is spent, or that has less than 10% of its quota left, is not warmed.
- Warming stops when the conversation changes underneath it: a new model, thinking level, or effort, a compaction, a revert, a rewind, `/clear`, a fork, or a session switch. It also stops 30 minutes after the last real request (in `idle` mode, 30 minutes after the run ends), and when Caudra exits.
- Each real request cancels the pending warm. Compaction, titles, goal checks, and other side requests never start one.
- One warm is in flight at a time, and a failed warm is not retried. A refused request turns warming off for the rest of the session.
- A warm that wrote the prompt again instead of reading it means the cache was already gone, so warming stops for the session. Warms since the last real request never cost more than the write they protect.

Warms appear in `/usage` under the `cache_warm` purpose and count toward session, goal, and autopilot budgets like any other request. They add nothing to the transcript.

The `Cache warming` block in `/usage` shows, for the session and for all time, how many warms were sent, what they cost, what they saved, and how often the request after a warm read the cache. A warm counts as a saving only when the next request came more than 30 seconds after the entry would have expired and read it.

While warming is off, Caudra still works out what it would have done and shows those rows as `estimated if enabled`, so you can compare before you turn it on. An estimate sends nothing. It is weighted by how often requests that arrived in time actually read the cache on the same model, and a model with too few such requests is left out of the estimate until it has more.

## Spend on a subscription

A Claude, ChatGPT, or Copilot login pays a flat monthly fee, so its turns never reach an invoice. Caudra still prices them at the provider's published API rates and files the figure separately, marked `≈`. That is what the same work would have cost through the API.

The status bar shows billed spend first and the subscription figure beside it: `$0.01 ≈$9.05`. A session on a subscription often has both, because [decision engine](/docs/permissions/#decision-engine-advice) calls bill to an API key. When the bar runs short of room, it keeps the larger figure, then drops spend. Hovering the figure shows both payers for the chat and the whole session. Clicking it opens `/usage`. When both figures are present, its session and lifetime totals place subscription spend below billed spend, aligned at the decimal point, with the legend on the same row. A single figure stays on the headline. The two amounts are never added together.

Turns recorded before Caudra tracked the two apart are filed as billed spend, so an older ledger can overstate what you paid.

## Lifetime spend

Deleting a session deletes its transcript. The record of what it cost lives in a separate ledger that no session owns, so trimming and forgetting leave your spending history intact.

Project totals use the exact directory recorded for each turn. [Bulk session migration](/docs/sessions/#moving-sessions-to-another-directory) includes historical project usage by default, with an option to leave it unchanged. Moving one session preserves its own counters without reattributing the shared ledger.

Press `g` in `/usage` to switch from this session to everything ever recorded: totals, the providers, accounts, models, projects, and purposes that cost the most, and a month by month breakdown. Turns recorded before Caudra tracked [accounts](/docs/accounts/) have none and stay out of the account list. Press `g` again to go back. The keys `/usage` takes are listed on its top border, where a click on `Ctrl+R reload`, `g`, or `e` does what the key does. Rest the pointer on a column, a figure, a heading, or a note to see what it means.

A long list of providers, models, projects, or months shows the 8 that cost the most and ends in `... and N more`. Click that line to show the rest, and `show less` to fold it again, or press `e` to open or close every list at once. Purposes always show in full.

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
| `memory` | Summarizing [project memory](/docs/memory/#summaries-and-spend) in the background |
| `autopilot` | [Autopilot](/docs/autopilot/) decisions about prompts, questions, plans, and stalls |
| `decision` | [Decision engine](/docs/providers/#decision-endpoints) calls: Auto mode, advice, screening, reranking, and `decide()` / `rerank()` |
| `tool_json_repair` | Isolated syntax repair for malformed tool arguments |
| `cache_warm` | [Cache warming](#cache-warming) refreshes, booked on the model whose cache they kept |

The model cannot answer that question on its own, because goals, compaction, and titles often run on the model already in use.

Two things worth knowing about the numbers:

- Runs started with `--ephemeral` leave no session behind, and their spend is still recorded and labelled, so the totals stay complete.
- A model with no published price contributes tokens but no cost. Caudra reports how many turns those were rather than counting them as free, so the total is a floor.
- Decision calls also count toward the session's cost, a goal's spend, and Autopilot's `max_cost`. A provider bills a decision call whether or not Caudra waits for it, so a call that timed out or was cancelled keeps reading its reply for up to 30 seconds and books what it cost.

The ledger holds one row per hour, model, project, and purpose, so it stays small on its own. [Retention](/docs/sessions/#retention) never touches it, and `--prune-older-than` is how you trim it.

## What the tools cost

Spend answers which models you paid for. `/tools` answers which tools filled the window they were paid for. It opens on the inventory, and `g`, or a click on the `g` hint on its top border, cycles through three recorded views before returning there.

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

Each server call a [`code`](/docs/mcp/#code-mode) run makes gets its own row, named `server.tool`, with its own calls, errors, and time. It adds no tokens: what the run returned to the model counts once, on the `code` row.

Token figures carry a `~` because they are estimates. Caudra measures them with the o200k tokenizer, scaled by what it [learned about each model's tokenizer](#watching-it-work), and it measures the result the model actually received, after [result limits](#smaller-results) trimmed it. Rows recorded before Caudra learned to scale them keep their unscaled counts. A tool with a small share of calls and a large share of tokens is the one to configure differently.

The percentiles are approximate for a different reason. A sum survives being merged across hours and projects while a percentile does not, so Caudra records the shape of each tool's durations as a small log-scale histogram rather than keeping every sample. Each figure names the top of the bucket a call landed in, which makes it an upper bound within roughly 12% of the real duration.

Tools that failed get a `Failures` block under the table naming the classes behind the rate: `cancelled`, `timed out`, `denied`, `not found`, `bad input`, and `failed` for the rest. A rate of 40% reads differently once you know it was `denied` every time. Each class is stated by the tool that failed rather than read from its output, so a command that fails on its own is `failed` whatever it printed.

Recorded activity lives in its own ledger, the same way spend does. Session counters go when the session goes. The project and global ledger holds one row per hour, tool, source, outcome, and project, so deleting a transcript leaves the record of what ran behind, and `caudra storage usage --prune-older-than` trims both ledgers in one pass.

Recording starts with this release. Earlier work is absent rather than counted as zero.
