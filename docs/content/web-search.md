---
title: "Web search"
description: "Choose the backends the websearch tool sends queries to, in what order, and how they share the work."
---

The `websearch` tool sends the model's queries to a search backend. With nothing configured it uses Exa's credential-free hosted MCP service, a third-party provider with anonymous rate limits. You can point it at your own SearXNG instance, at a paid API, or at several backends at once.

Search settings live in the `[websearch]` table of the global `caudra.toml`. A project `caudra.toml` cannot set them, because they hold credentials and decide where your queries go. A project that sets `[websearch]` fails to load with an error that says so.

## Backends

| Backend | Name | Needs |
|---------|------|-------|
| Exa MCP | `exa-mcp` | Nothing. The default. |
| SearXNG | `searxng` | `url`, and at most one of `api_key`, `bearer_token`, or `user` with `password` |
| Exa API | `exa` | `api_key` |
| Brave | `brave` | `api_key` |
| Kagi | `kagi` | `api_key` |
| SerpApi | `serpapi` | `api_key`, and `engine` set to `google` or `bing` |

Each backend reads its settings from its own table, such as `[websearch.searxng]`. A key you leave out reads the environment variable named in the [configuration reference](/docs/configuration/#websearch), such as `SEARXNG_URL` or `BRAVE_API_KEY`. Caudra reads those from its own environment or the global `.env`, never from a project `.env`, so a repository cannot redirect your searches.

Write credentials as references rather than in the file:

```toml
[websearch]
backends = ["searxng", "exa-mcp"]

[websearch.searxng]
url = "https://search.example.net"
api_key = "{env:SEARXNG_API_KEY}"
```

`{file:PATH}` reads a value from a file. See [Values from files](/docs/configuration/#values-from-files).

## Order and mode

`backends` lists the backends in the order you prefer them. A backend that is not set up correctly, such as Brave without a key, is skipped, and Caudra warns with its name and what is missing. The warning never shows a value. When no listed backend is ready, `websearch` tells the model why the first one is not.

`mode` decides how the ready backends share the one `websearch` tool:

| Mode | What happens |
|------|--------------|
| `fallback` (default) | The first backend answers. When its request fails, such as a timeout, an HTTP error, or a reply that does not parse, the next one takes over. The result names the backend that answered and those that failed before it. |
| `single` | Only the first ready backend answers. |
| `all` | The tool gets a `backend` argument that lists the ready backends, so the model can pick one per call. A call without it uses the fallback order. |

A search that finds nothing is an answer, so it never falls back. A cancelled search stops at once. The model sees the parameters of the first backend, such as SearXNG's `categories`, and a fallback backend receives only the parameters it supports. In `all` mode the tool offers the parameters of every ready backend.

Fallback can send a query to a backend you did not mean as your main one. When the first backend is a private SearXNG instance and the next is `exa-mcp`, a query reaches Exa whenever SearXNG fails. List only the backends you are willing to send every query to.

## Precedence

| Setting | First of |
|---------|----------|
| Backends | `[websearch] backends`, `WORKCELL_WEBSEARCH_BACKEND` as a comma-separated list, `exa-mcp` |
| Mode | `[websearch] mode`, `WORKCELL_WEBSEARCH_MODE`, `fallback` |
| A backend's setting | its key in `[websearch.<backend>]`, its environment variable |

`backends = []` turns `websearch` off, and so does `WORKCELL_WEBSEARCH_BACKEND=disabled`. `webfetch` keeps working either way.

## Edit from Caudra

`/settings` opens on the Web search tab. It lists every backend with whether it is ready and where its settings come from: the config, the environment, or nowhere. Check a backend with `Space` to use it, move it up or down with `[` and `]` to change its place in the order, and click a mode or press `m` to switch to it. `d` clears your list so the default applies again. Opening a backend with `Enter` shows a form for its settings. Credentials stay masked, and saving one as plain text asks first, as `/env` does. Caudra writes the global `caudra.toml` and keeps your comments.

When Caudra runs its tools on a remote Workcell, the tab is read-only, since web tools run there.

## When changes apply

While the terminal UI runs, a saved edit to `[websearch]` applies at once, in every open session, and so does a change to the global `.env` that one of its `{env:NAME}` references reads. The footer says which backends are now in use. A save that changes no setting keeps searches already waiting for your approval valid. One that changes a setting makes them fail, and the model searches again. Headless runs and ACP read the settings at startup. See [Hot reload](/docs/configuration/#hot-reload).

These settings drive the Workcell embedded in Caudra. A [remote workspace](/docs/remote-workspaces/) or [sandbox](/docs/sandboxes/) runs its own Workcell, which reads `WORKCELL_WEBSEARCH_BACKEND` and the other variables from its own environment.

Every result passes through content screening like any other web output, whichever backend returned it. See [the decision engine](/docs/permissions/#decision-engine-advice).
