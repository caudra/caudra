---
title: "Code"
description: "The code tool: Python snippets for computation, search over past conversations, MCP servers, and decision helpers."
---

The model has one tool for running Python, `code`. A snippet computes, reshapes data, and reaches what the session connects it to: the [stored history](#search-past-conversations) of your conversations, your [MCP servers](/docs/mcp/#code-mode), and, with the decision engine on, the [decision helpers](#decisions-and-reranking). It filters and summarises before anything reaches the model, so a search over hundreds of results can come back as three lines.

```python
hits = await history.search("flaky retry test", since="2026-09-01")
[(hit["when"], hit["title"], hit["snippet"]) for hit in hits["hits"][:3]]
```

`code` is offered in every session, with or without MCP servers. Its definition never changes, so connecting a server, enabling the decision engine, or switching to plan mode leaves the [prompt cache](/docs/configuration/#the-prompt-cache) intact. The [Tools](/docs/tools/#code) page lists its parameters.

## Snippets

A snippet is module code rather than a function body. Its last expression is the result, and the model receives it together with anything the snippet printed. Top-level `await` works. `return` at the top level is a mistake, and Caudra drops it from a last line, as [Results and errors](/docs/mcp/#results-and-errors) describes along with its other repairs.

A snippet that calls nothing outside the interpreter has nothing to ask about, so it runs without a prompt. Each call it makes to `history`, an MCP server, or a decision helper is [reviewed on its own](#permissions). A failed call raises `RuntimeError` and a refused one raises `PermissionError`, and the snippet can catch either and carry on. Calls that ran before a failure keep their effects.

Up to 8 calls run at once, and one run makes at most 100. Run independent calls together with `asyncio.gather`.

## Variables between runs

Variables that a run defines are still there in the next run of the same agent. The main agent and each subagent keep their own. They live in memory, up to 16 MiB. Past that they are dropped and the next run starts empty. They do not survive a restart of Caudra, and a resumed, rewound, or forked conversation starts without them. The model can pass `reset: true` to start a run empty. When variables are lost, the result says so.

## The Python subset

Snippets run in a Python subset on the bundled Monty worker. There is no filesystem, network, subprocess, class inheritance, or third-party package. Host clocks, unseeded randomness, and sleep are refused. The standard modules are `asyncio`, `base64`, `binascii`, `collections`, `copy`, `dataclasses`, `datetime`, `functools`, `itertools`, `json`, `math`, `os`, `pathlib`, `random`, `re`, `sys`, `time`, `typing`, and `unicodedata`.

Before a snippet runs, it is type-checked against the signatures of what it calls. Type checking can reject `map`, `filter`, `getattr`, `setattr`, `hasattr`, and `format`, so comprehensions and f-strings work better.

## Where snippets run

Snippets always run on this machine, even when the session's other tools run on a [remote Workcell](/docs/remote-workspaces/). Your MCP servers and your stored history are both here.

> **Note**
>
> **Snippets run in a worker process of their own, not inside Caudra.** The worker has no filesystem, network, subprocess, or environment access. A snippet reaches the outside only through the calls Caudra answers, and each call passes a permission check. A snippet that crashes the worker ends only that process, and Caudra starts a new one. This is process isolation rather than an operating-system boundary: the worker runs as your user.

A run has 30 seconds of computation. The clock stops while the snippet waits for a call, so a snippet that waits on slow servers still gets its full 30 seconds of its own work. A runaway loop stops when the time is up.

One agent's runs go one at a time, because they share the agent's variables. To run work in parallel, the model gathers it inside one snippet. A `code` call also waits until the model's response is complete before it starts, even when other tools start while the response streams. A response that is retried or abandoned therefore never leaves a run behind.

Release builds, Nix, and `make install` from a checkout bundle the worker. A build without it warns at startup that the `code` tool is unavailable, and a call returns an error that says so. When MCP servers are configured, the TUI shows the same reason as it starts, and `/mcp` shows it above the server list. `WORKCELL_MCP_CODE_WORKER` can point Caudra at a worker binary.

## In the transcript

A `code` card shows the snippet, numbered and highlighted as Python. Below it comes a row for each call the snippet made, labelled like `history › search` or `github › list_issues` with its status, then what the snippet printed, the value it returned, and why it stopped when it did not finish. The rows fill in while the snippet runs, and opening a row shows that call's own card. Output that runs long scrolls in a window of [`ui.scroll_card_lines`](/docs/configuration/#ui) rows, like a `shell` card. MCP call cards are described in [Results in the transcript](/docs/mcp/#results-in-the-transcript).

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

With the experimental [decision engine](/docs/configuration/#decisions) on and the `history_search` feature set to `enforce`, Caudra reranks:

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

The model learns the servers and their tools from a `# MCP servers` reminder, and `search()` finds the tools the reminder leaves out. [Code mode](/docs/mcp/#code-mode) covers the server list, calls, results, resources, and naming. A server named after a module, such as `history`, gets a trailing `_`: `history_`.

## Decisions and reranking

With the [decision engine](/docs/configuration/#decisions) on and a base URL set, snippets get two more functions, `decide()` and `rerank()`. They work on what a snippet gathered: a snippet calls a server or searches history, then judges, filters, or sorts the result before any of it reaches the model, or branches on a judgement without another round trip.

```python
issues = await github.list_issues(owner="caudra", repo="caudra", state="open")
titles = [issue["title"] for issue in issues]
ranked = await rerank("crashes when saving a file", titles, threshold=0.5, top_k=10)
[issues[entry["index"]]["number"] for entry in ranked]
```

| Function | What it returns |
|---|---|
| `await decide(input, questions, model=None, timeout_ms=None)` | `{"model", "answers", "by_name"}`: typed answers about `input` |
| `await rerank(query, candidates, threshold=None, top_k=None, model=None, timeout_ms=None)` | `[{"index", "score"}]`: the candidates relevant to `query`, best first |

`decide()` takes the same questions as a workflow's [`decide()`](/docs/workflows/#typed-decisions): a list of `predicate`, `choice`, and `score` questions in the shape of OpenAI Decisions, each with a `name`. `input` is the state the questions are about, and it must stay within 1,500 bytes as JSON. Read an answer by its question's name, as in `result["by_name"]["urgent"]["probability"]`.

`rerank()` judges up to 256 candidates, each a string or a small dict. `index` is a candidate's position in `candidates`, and `score` is the probability that it is relevant. `threshold` drops candidates below it, and `top_k` keeps at most that many. Candidates go out 16 to a request, with up to 4 requests at once. Each request waits up to [`rerank_timeout_ms`](/docs/configuration/#decisions), like Caudra's own reranking, unless the call passes a shorter `timeout_ms`. A `decide()` call waits up to `timeout_ms` from the configuration. An empty list returns `[]` without a request.

The model learns about both functions from the `# MCP servers` reminder, which describes them while the engine is on, with or without servers. Without the engine they do not exist, and a snippet that calls one raises `NameError`.

The reminder also tells the model how to get reliable answers, following the habits of Caudra's own question sets:

- Keep `input` to the fields the questions judge, and name the field in each question: "Does `body` report a crash?".
- Write `instructions` as a map when a question has edge cases: `{"question": "...", "focus": "what counts and what does not"}`.
- Give each choice and level a `description` that sets it apart from the others.
- Write the rerank `query` as the need rather than keywords, and send each candidate as a short label and summary. `index` maps back to the full item.
- Act on a predicate at about 0.85 or more. Below 0.6 the answer is a guess.
- Judge one item per call. `decide()` answers each question once for the whole `input`, so a list of items gets one call per item, with every question about that item in the call. A local engine answers overlapping calls one at a time, and each call waits only `timeout_ms`, so a snippet that runs many calls under `asyncio.gather` passes a larger `timeout_ms`.

A call needs no approval and works in plan mode, since it only reads. A malformed question or argument raises `TypeError` or `ValueError` before anything is sent. An unreachable engine, a timeout, a refusal, or an invalid answer raises `RuntimeError`, and one failed request fails the whole `rerank()`. The opt-in [decision log](/docs/permissions/#decision-engine-advice) records each call under the feature `code`. Answers are data for the snippet: they never grant a permission.

The card shows each call as a row. A `decide()` row lists its verdicts, such as `severity high 82.4% · parser_bug yes 91.0%`, and a `rerank()` row says how many candidates it kept and names the best one. Both rows always show in full: a `decide()` row shows each question with a bar for every option. The likeliest option is highlighted, and a winner below 60% is marked uncertain alongside the runner-up. A `rerank()` row shows the candidates it kept, best first, each with its score and the label it was given. Click the row's footer to switch to the JSON the snippet received, and click it again to go back. Selecting any part of either view copies that JSON whole.

## Permissions

Starting a run never asks: the `code` tool is allowed by default, because a snippet on its own reaches nothing. What the snippet calls is reviewed call by call:

| Call | Review |
|---|---|
| An MCP tool | By its `server.tool` name, under the [MCP rules](/docs/permissions/#mcp-tool-calls) |
| `read_resource()` | Asks, scoped to the server and URI. `resources()` never asks |
| `history` in the current project | Allowed |
| `history` in another project, or `scope="all"` | Asks, as [Privacy and permissions](#privacy-and-permissions) describes |
| `decide()`, `rerank()` | Allowed |

A deny rule for `code` itself blocks every run.
