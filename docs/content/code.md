---
title: "Code"
description: "The code tool: Python snippets for computation and the modules they reach, from past conversations and MCP servers to images, workflows, and the decision engine."
---

The model has one tool for running Python, `code`. A snippet computes, reshapes data, and reaches what the session connects it to through [modules](#modules): the [stored history](#search-past-conversations) of your conversations, your [MCP servers](/docs/mcp/#code-mode), [image generation, the host description, workflows, and automations](#tools-reached-from-code), and the [decision engine](#decisions-and-reranking). It filters and summarises before anything reaches the model, so a search over hundreds of results can come back as three lines.

```python
hits = await history.search("flaky retry test", since="2026-09-01")
[(hit["when"], hit["title"], hit["snippet"]) for hit in hits["hits"][:3]]
```

`code` is offered in every session and cannot be turned off. Its definition never changes, so connecting a server, signing in, switching decision endpoints, or switching to plan mode leaves the [prompt cache](/docs/configuration/#the-prompt-cache) intact. The [Tools](/docs/tools/#code) page lists its parameters.

## Snippets

A snippet is module code rather than a function body. Its last expression is the result, and the model receives it together with anything the snippet printed. Top-level `await` works. `return` at the top level is a mistake, and Caudra drops it from a last line, as [Results and errors](/docs/mcp/#results-and-errors) describes along with its other repairs.

A snippet that calls nothing outside the interpreter has nothing to ask about, so it runs without a prompt. Each call it makes to a module is [reviewed on its own](#permissions). A failed call raises `RuntimeError` and a refused one raises `PermissionError`, and the snippet can catch either and carry on. Calls that ran before a failure keep their effects.

Up to 8 calls run at once, and one run makes at most 100. Run independent calls together with `asyncio.gather`.

## Modules

A module is a namespace of calls that reach beyond the interpreter. Each call is a function to `await`, unless its help says otherwise.

| Module | Calls |
|---|---|
| `history` | `search`, `read`, `sessions`: [past conversations](#search-past-conversations) |
| `mcp` | `search`, `servers`, `resources`, `read_resource`. Each MCP server is also a namespace of its own, such as `github` |
| `decisions` | `decide`, `rerank`: [judgements from the decision engine](#decisions-and-reranking) |
| `image` | `generate`: a raster image, as [Image generation](#image-generation) describes |
| `host` | `environment`: what the machine commands run on offers |
| `workflow` | `list`, `validate`, `start`, `status`, `inspect`, `history`, `pause`, `resume`, `stop` |
| `automation` | `list`, `validate`, `history` |

`help` is the one other name a snippet starts with. The `code` description carries one line per module, and `help` loads the rest when the model needs it:

```python
help()                    # every module, and whether this session can reach it now
help("workflow")          # a module's page with every call's signature
help("image.generate")    # one call's full reference
help("github")            # an MCP server's tools and their signatures
```

A module the session cannot reach still exists, so a snippet reads the same everywhere. Its calls raise `RuntimeError` with the fix, such as signing in or enabling a tool group, and `help()` lists the same reason.

Four calls take arguments that are easy to get wrong, so their reference comes first: `history.search`, `history.read`, `image.generate`, and `workflow.start`. The `code` description marks them. The first such call in a conversation does not run. It raises `RuntimeError` carrying that call's full reference, with no transcript row, prompt, or spend, and the same call made again runs. Reading `help` for the call or its module first lets the first call run at once. After a compaction, or in a resumed or forked session, the next call asks again, because the reference is no longer in the context. A subagent keeps its own record. Every other call runs without it.

## Run time

A run stops after `timeoutSec` seconds, 120 by default and at most 3600, counted on the wall clock across computation, waits, and calls. The model raises it for a snippet that polls. A run that times out keeps the rows of the calls it made and the variables from before it ran. What it printed and assigned is lost, and the model reads that it ran past `timeoutSec`.

## Variables between runs

Variables that a run defines are still there in the next run of the same agent. The main agent and each subagent keep their own. They live in memory, up to 16 MiB. Past that they are dropped and the next run starts empty. They do not survive a restart of Caudra, and a resumed, rewound, or forked conversation starts without them. The model can pass `reset: true` to start a run empty. When variables are lost, the result says so.

## The Python subset

Snippets run in a Python subset on the bundled Monty worker. There is no filesystem, network, subprocess, class inheritance, or third-party package, and unseeded randomness is refused. The standard modules are `asyncio`, `base64`, `binascii`, `bisect`, `collections`, `copy`, `csv`, `dataclasses`, `datetime`, `functools`, `hashlib`, `itertools`, `json`, `math`, `os`, `pathlib`, `posixpath`, `random`, `re`, `sys`, `time`, `tomllib`, `typing`, and `unicodedata`.

Clocks read the host clock: `time.time()`, `time.monotonic()`, `time.perf_counter()`, and `datetime.now()` answer, and the time zone is UTC. `time.sleep()` and `await asyncio.sleep()` wait for real, up to the run's `timeoutSec`. `asyncio.sleep()` runs beside calls gathered with it, so a snippet can poll:

```python
import asyncio
while (await workflow.status(run_id))["status"] == "active":
    await asyncio.sleep(10)
```

While a snippet waits, its card notes it, such as `waiting 10s`. `Esc` stops a wait at once. The Workcell `code` tool outside Caudra still refuses clocks and sleep.

Before a snippet runs, it is type-checked against the signatures of what it calls. Type checking can reject `map`, `filter`, `getattr`, `setattr`, `hasattr`, and `format`, so comprehensions and f-strings work better.

## Where snippets run

Snippets always run on this machine, even when the session's other tools run on a [remote Workcell](/docs/remote-workspaces/). Your MCP servers and your stored history are both here.

> **Note**
>
> **Snippets run in a worker process of their own, not inside Caudra.** The worker has no filesystem, network, subprocess, or environment access. A snippet reaches the outside only through the calls Caudra answers, and each call passes a permission check. A snippet that crashes the worker ends only that process, and Caudra starts a new one. This is process isolation rather than an operating-system boundary: the worker runs as your user.

A run has 30 seconds of computation. The clock stops while the snippet waits for a call or sleeps, so a snippet that waits on slow servers still gets its full 30 seconds of its own work. A runaway loop stops when the time is up. [`timeoutSec`](#run-time) bounds the whole run on top of that.

One agent's runs go one at a time, because they share the agent's variables. To run work in parallel, the model gathers it inside one snippet. A `code` call also waits until the model's response is complete before it starts, even when other tools start while the response streams. A response that is retried or abandoned therefore never leaves a run behind.

Release builds, Nix, and `make install` from a checkout bundle the worker. A build without it warns at startup that the `code` tool is unavailable, and a call returns an error that says so. When MCP servers are configured, the TUI shows the same reason as it starts, and `/mcp` shows it above the server list. `WORKCELL_MCP_CODE_WORKER` can point Caudra at a worker binary.

## In the transcript

A `code` card shows the snippet, numbered and highlighted as Python. Below it comes a row for each call the snippet made, labelled like `history › search` or `github › list_issues` with its status, then what the snippet printed, the value it returned, and why it stopped when it did not finish. The rows fill in while the snippet runs. A [Home Assistant](/docs/mcp/#home-assistant) or [Executor](/docs/mcp/#executor) row shows its call's card without a click, and a click folds it. Opening any other row shows that call's own card. A row for a [tool reached from `code`](#tools-reached-from-code) opens to the card the tool draws on its own, such as the image thumbnail of `image.generate`. A dict or a list that the snippet returned, or printed as `print(r)` or `json.dumps(r)` writes it, either on a line of its own or as all it printed, shows as a folded tree: one row such as `▸ {…} 7 keys`. A click on that row opens the whole tree, a click on any `{` or `[` row inside folds or opens that part, and a click on the first row folds the tree again. The [raw view](/docs/mcp/#results-in-the-transcript) of an MCP call's card draws a dict or a list the same way, open. Any other output, such as `print(len(hits))` or a formatted line, shows as text. When the tree is only what a call answered, such as `print(await host.environment())`, that call's card also shows without a click, and a click folds it. The model still reads everything the snippet printed, as text. Output that runs long scrolls in a window of [`ui.scroll_card_lines`](/docs/configuration/#ui) rows, like a `shell` card. When the output holds a tree, the card instead draws it whole, opened trees included, and holds back only long text. MCP call cards are described in [Results in the transcript](/docs/mcp/#results-in-the-transcript).

A running task card lists each step its subagent has taken, newest last. A subagent's `code` run shows there as the calls it makes: `code 3 calls`, with the 8 newest beneath it, each written as the snippet made it, such as `history.search(query="flaky test", scope="all")`, and marked by its status. A run that makes no calls shows its snippet instead: a one-line snippet shows that line, and a longer one shows its first statement and how many lines it has.

## Search past conversations

The `history` module searches the conversations stored on this machine: what you wrote, what the model answered and reasoned, the tools it called, and what they returned. It includes the parts of a conversation that compaction removed from view, and the transcripts of its subagents. The model uses it to find an earlier decision, a command that worked, or the session where a bug was last seen.

```python
found = await history.search("why we pinned the sqlite version", scope="all")
first = found["hits"][0]
await history.read(first["ref"], around=2)
```

### Functions

| Function | What it returns |
|---|---|
| `await history.search(query, scope="project", kinds=None, tools=None, since=None, until=None, sort="relevance", rerank=None, limit=None, offset=0)` | `{"hits", "ranking", "judged_unrelated", "more", "unindexed_sessions", "note"}` |
| `await history.read(ref, offset=0, limit=8000, around=0)` | One item's full text, from byte `offset`, with up to `around` neighbouring items on each side |
| `await history.sessions(scope="project", title=None, limit=20, offset=0)` | Sessions in scope, most recently updated first, each with `session`, `title`, `project`, `updated`, `items`, and `subagent_items`. `title` keeps sessions whose title contains it, in any letter case, and `limit` stops at 100 |

`search()` arguments:

- `query` is free text. Each word is matched on its own, and `"a quoted phrase"` is matched whole. Only the first 16 words or phrases count.
- `kinds` keeps only `user`, `assistant`, `summary`, `reasoning`, `tool_call`, or `tool_result` items. A `summary` is a compaction summary.
- `tools` keeps only calls and results of the named tools, such as `["shell"]`.
- `since` and `until` take an ISO date or datetime, such as `2026-10-16` or `2026-10-16T09:30:00Z`. A time without an offset is UTC.
- `sort="newest"` orders matches by time instead of relevance.
- `rerank=False` skips [reranking](#ranking) for this call.
- `limit` defaults to 10 after reranking and 20 without, and stops at 50. `offset` pages through the rest.

Each hit has `ref`, `session`, `title`, `project`, `when`, `kind`, `tool`, `agent`, `snippet`, and `score`. `ref` is an opaque string to pass to `read()`. `agent` names the subagent whose transcript holds the item, and is `None` for the main conversation. `snippet` is up to 320 bytes around the densest cluster of matched words, each match marked as `«match»`. `when` is UTC.

`read()` returns the item's `text` with `offset`, `total_bytes`, and `next_offset` for the next page, up to 64 KiB at a time. `before` and `after` preview up to 20 neighbouring items each, 200 bytes apiece.

### Scopes

| `scope` | Searches |
|---|---|
| `"session"` | This conversation, with its compacted parts and subagents |
| `"project"` | Every session of this project: its main checkout and its linked worktrees, or the remote workspace of a remote session |
| `"all"` | Every stored session on this machine |
| A path | The project at that path, with its checkout and linked worktrees. A relative path starts at the current project directory. A remote session needs an absolute path |

A conversation that was forked appears in both sessions. Search keeps each item once, from the session updated most recently.

### What is indexed

Search runs on a full-text [index](/docs/sessions/#search-index) in the session database, `caudra.db` in your [state directory](/docs/configuration/#directory-layout). The index stores no copy of the text, so snippets and `read()` come from the stored conversation itself.

| Item | Indexed text |
|---|---|
| Your messages | The text, without the `<system-reminder>` blocks Caudra adds. Reminders Caudra injected on their own are left out |
| Assistant text | The text. A compaction summary is a `summary` |
| Reasoning | The text, unless the provider redacted it |
| Tool calls | The tool name and the string values of its input, up to 4 KiB |
| Tool results | The first 12 KiB and the last 4 KiB of the result text |

Words are stemmed and accents are folded, so `retrying` finds `retry` and `café` finds `cafe`.

Before each query, Caudra indexes what changed in the query's scope for up to 1.5 seconds. What is still not indexed after that counts in `unindexed_sessions`, and a later query finds it. The first search over a large history can therefore report unindexed sessions while the TUI's background indexing catches up.

### Ranking

Without reranking, matches come back in keyword order. Items that contain every word come first, and items that contain some of them fill any shortfall. Within each tier, the BM25 score orders them, weighted by kind: your messages and compaction summaries count most, then assistant text, reasoning, tool calls, and tool results. `score` is that weighted BM25 score, and `ranking` is `"keyword"`.

With a [decision endpoint](/docs/providers/#decision-endpoints) active and the `history_search` feature set to `enforce`, Caudra reranks:

1. Keyword search picks a pool of up to 64 matches, at most 8 from any one session when the scope spans sessions. With `sort="newest"`, the pool is the 64 newest matches.
2. The engine judges each match against the query: does this excerpt help answer it? It answers 16 matches to a request, all four requests at once. Because an engine may answer one request at a time, in any order, each request waits up to one [`rerank_timeout_ms`](/docs/configuration/#decisions) per request in the pool, so four requests wait up to four budgets.
3. Matches below `thresholds.relevance_floor` are dropped. The rest come back best first, or in time order with `sort="newest"`, 10 to a page by default.

After reranking, `ranking` is `"reranked"`, `score` is the probability of relevance, `judged_unrelated` counts the dropped matches, and `more` says whether kept matches remain in the pool. When every match was judged unrelated, `hits` is empty and `note` suggests a new query or `rerank=False`.

The engine receives the query and, for each match, its kind, tool, date, session title, and an excerpt of up to 600 bytes, redacted and cut before it is sent. When the engine fails or times out, the search returns keyword order and `note` says so. In `shadow` the engine is still asked and its answer goes to the [decision log](/docs/permissions/#decision-engine-advice), and the search returns keyword order.

```toml
[decisions.features]
history_search = "enforce"
```

The card's row summarises what happened, such as `reranked 64 keyword matches, kept 12, showing 10` or `keyword order: 20 of 143`.

### Privacy and permissions

The current project's history needs no approval: its checkout and linked worktrees, or the remote workspace of a remote session. `/cd` moves the current project with it. Another project and `scope="all"` ask first, and so does a `read()` of an item from another project. The prompt shows the call as the snippet made it, such as `history.read(ref="…", limit=5000)`, and what it reads: which session, or which project's history it searches. Choices 2 and 3 remember reading that project's history, or the history of all projects, rather than the one call. [Session history](/docs/permissions/#session-history) lists the rules.

History calls only read, so they work in [plan mode](/docs/permissions/#plan-mode) and in [read-only agents](/docs/permissions/#read-only-agents). Where no one can answer a prompt, a read of another project's history is denied unless a rule allows it. A session that is not saved, such as an `--ephemeral` one, has no history to search, and every call raises `RuntimeError`.

> **Note**
>
> **Results are not redacted.** A tool result that printed a token can come back in a snippet or a `read()`. Other projects ask first for this reason. Only the excerpts sent to the decision engine for reranking are redacted.

## MCP servers

Each enabled MCP server is a Python namespace, and each of its tools is an async function of that namespace:

```python
issues = await github.list_issues(owner="caudra", repo="caudra")
[issue["title"] for issue in issues if issue["comments"] > 10]
```

The model learns the servers and their tools from a `# MCP servers` reminder, and `mcp.search()` finds the tools the reminder leaves out. [Code mode](/docs/mcp/#code-mode) covers the server list, calls, results, resources, and naming. A server named after a module, such as `history`, gets a trailing `_`: `history_`.

## Decisions and reranking

With a [decision endpoint](/docs/providers/#decision-endpoints) active, the `decisions` module answers `decisions.decide()` and `decisions.rerank()`. They work on what a snippet gathered: a snippet calls a server or searches history, then judges, filters, or sorts the result before any of it reaches the model, or branches on a judgement without another round trip.

```python
issues = await github.list_issues(owner="caudra", repo="caudra", state="open")
titles = [issue["title"] for issue in issues]
ranked = await decisions.rerank("crashes when saving a file", titles, threshold=0.5, top_k=10)
[issues[entry["index"]]["number"] for entry in ranked]
```

| Function | What it returns |
|---|---|
| `await decisions.decide(input, questions, model=None, timeout_ms=None)` | `{"model", "answers", "by_name"}`: typed answers about `input` |
| `await decisions.rerank(query, candidates, threshold=None, top_k=None, model=None, timeout_ms=None)` | `[{"index", "score"}]`: the candidates relevant to `query`, best first |

`decide()` takes the same questions as a workflow's [`decide()`](/docs/workflows/#typed-decisions): a list of `predicate`, `choice`, and `score` questions in the shape of OpenAI Decisions, each with a `name`. `input` is the state the questions are about, and it must stay within 1,500 bytes as JSON. Read an answer by its question's name, as in `result["by_name"]["urgent"]["probability"]`.

`rerank()` judges up to 256 candidates, each a string or a small dict. `index` is a candidate's position in `candidates`, and `score` is the probability that it is relevant. `threshold` drops candidates below it, and `top_k` keeps at most that many. Candidates go out 16 to a request, with up to 4 requests at once. Each request waits up to [`rerank_timeout_ms`](/docs/configuration/#decisions), like Caudra's own reranking, unless the call passes a shorter `timeout_ms`. A `decide()` call waits up to `timeout_ms` from the configuration. An empty list returns `[]` without a request.

The model learns how to use both from the `# MCP servers` reminder, which describes them while an endpoint is active, with or without servers. Without an endpoint, a call raises `RuntimeError` that names `/decision-model`.

The reminder also tells the model how to get reliable answers, following the habits of Caudra's own question sets:

- Keep `input` to the fields the questions judge, and name the field in each question: "Does `body` report a crash?".
- Write `instructions` as a map when a question has edge cases: `{"question": "...", "focus": "what counts and what does not"}`.
- Give each choice and level a `description` that sets it apart from the others.
- Write the rerank `query` as the need rather than keywords, and send each candidate as a short label and summary. `index` maps back to the full item.
- Act on a predicate at about 0.85 or more. Below 0.6 the answer is a guess.
- Judge one item per call. `decide()` answers each question once for the whole `input`, so a list of items gets one call per item, with every question about that item in the call. A local engine answers overlapping calls one at a time, and each call waits only `timeout_ms`, so a snippet that runs many calls under `asyncio.gather` passes a larger `timeout_ms`.

A call needs no approval and works in plan mode, since it only reads. A malformed question or argument raises `TypeError` or `ValueError` before anything is sent. An unreachable engine, a timeout, a refusal, or an invalid answer raises `RuntimeError`, and one failed request fails the whole `rerank()`. The opt-in [decision log](/docs/permissions/#decision-engine-advice) records each call under the feature `code`. Answers are data for the snippet: they never grant a permission.

The card shows each call as a row. A `decide()` row lists its verdicts, such as `severity high 82.4% · parser_bug yes 91.0%`, and a `rerank()` row says how many candidates it kept and names the best one. Both rows always show in full: a `decide()` row shows each question with a bar for every option. The likeliest option is highlighted, and a winner below 60% is marked uncertain alongside the runner-up. A `rerank()` row shows the candidates it kept, best first, each with its score and the label it was given. Click the row's footer to switch to the JSON the snippet received, and click it again to go back. Selecting any part of either view copies that JSON whole.

## Tools reached from `code`

Four built-in tools are not offered to the model on their own. The model calls them as module calls, which keeps them out of every request until they are needed:

| Call | Runs the tool |
|---|---|
| `await image.generate(prompt, out, quality=None, size=None, images=None)` | `image_generate` |
| `await host.environment()` | `execution_environment` |
| `await workflow.<action>(...)` | `workflow` |
| `await automation.<action>(...)` | `automation` |

A module call runs its tool exactly as a direct call did. The tool keeps its permission rules, its [tool group](/docs/system-prompts/#tool-groups), plan mode and read-only checks, change records, [usage](/docs/context/#inspect-the-active-window) under its own name, and its card. `host.environment()` describes the machine the session's commands run on, a [remote Workcell](/docs/remote-workspaces/) included. The snippet receives a dict: the saved `path` of an image, the host's facts and commands, or the workflow and automation runtime's own records.

A direct call of one of these tools, on its own or inside `batch`, fails with the module call to use instead.

### Image generation

`image.generate()` needs a ChatGPT subscription login (`caudra auth login openai`) and bills against that plan. It saves one PNG per call and never overwrites a local file, so read the returned `path`. `help("image.generate")` lists the size rules and how to pass reference images.

## Permissions

Starting a run never asks: the `code` tool is allowed by default, because a snippet on its own reaches nothing. What the snippet calls is reviewed call by call:

| Call | Review |
|---|---|
| An MCP tool | By its `server.tool` name, under the [MCP rules](/docs/permissions/#mcp-tool-calls) |
| `read_resource()` | Asks, scoped to the server and URI. `resources()` never asks |
| `history` in the current project | Allowed |
| `history` in another project, or `scope="all"` | Asks, as [Privacy and permissions](#privacy-and-permissions) describes |
| `decisions.decide()`, `decisions.rerank()` | Allowed |
| `image`, `host`, `workflow`, `automation` | Under the rules of the tool the call runs, such as `image_generate`. The prompt shows the call as the snippet made it |
| `help()`, `mcp.search()`, `mcp.servers()` | Allowed |

A deny rule for `code` itself blocks every run. `code` cannot be disabled with `disabled_tools` or a profile, because the modules live in it. Turn off a module's [tool group](/docs/system-prompts/#tool-groups) instead.
