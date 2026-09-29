+++
title = "Logging"
weight = 14
[extra]
group = "Reference"
+++

# Logging

Caudra writes one structured JSON record per line to a rotating file. It is on by
default, stays on your machine, and is the first thing to read when a run does
something you did not expect.

Read it three ways:

- `/logs` in the TUI, for a live view with fuzzy filtering and expansion.
- `caudra logs` in a shell, for piping into `jq` or pasting into a bug report.
- Any file reader, since each line is plain JSON.

In `/logs`, the wheel scrolls the page and clicking the level in the footer
cycles it. `/` opens a filter field where each term matches as a subsequence
against the message, the target, the level, and each field on its own. Space
separates terms and a record has to match all of them, so `provider retry` finds
a retry from the provider.

A record shows every field it carries, so a wide one runs past the right margin.
`w` wraps it onto as many rows as it needs and the arrow keys pan across it, the
same pair of answers the [Workbench](/docs/workbench/) editor gives a long line.
The arrow hint appears only while there is something out there to reach.

`Tab` on a selected record keeps only the records sharing its narrowest id, which
turns a scattered turn or tool call into a readable sequence. A hint row lists
the rest. See [Commands](/docs/commands/#logs) for the keys.

## Where the file lives

| Platform | Path |
|----------|------|
| Linux | `~/.local/state/caudra/logs/caudra.log` |
| macOS | `~/Library/Application Support/caudra/logs/caudra.log` |
| Windows | `%APPDATA%\caudra\logs\caudra.log` |

Rotated files sit next to it as `caudra.1.log`, `caudra.2.log`, and so on, with
the highest number being the oldest. `storage.max_log_bytes_mb` sets when
rotation happens and `storage.max_log_files` sets how many to keep. Both are in
[Configuration](/docs/configuration/#storage).

Every Caudra process on the machine appends to the same file, so a record from a
background session can appear between two records from the one in front of you.
The `session_id` field tells them apart.

## What a record looks like

```json
{
  "timestamp": "2026-09-09T14:22:07.418123Z",
  "level": "WARN",
  "target": "caudra::provider",
  "fields": {
    "message": "request failed, retrying",
    "event": "retry",
    "attempt": 3,
    "delay_ms": 4000,
    "status": 529
  },
  "spans": [{ "name": "turn", "session_id": "01J8...", "turn_id": 4 }]
}
```

`target` says which part of Caudra spoke. Events Caudra raises on purpose use one
of `caudra::agent`, `caudra::provider`, `caudra::tool`, `caudra::mcp`,
`caudra::permission`, or `caudra::session`, and those are the ones worth
filtering on. Anything else, including records from dependencies, carries the
Rust module path it came from, such as `isahc::handler`.

`fields.event` names the specific thing that happened, and `fields.outcome` is
one of `ok`, `error`, `cancelled`, or `timeout`.

`spans` carries the correlation ids. A record produced while a turn was running
has `session_id` and `turn_id`, and a record produced inside a tool call adds
`tool` and `tool_use_id`. To follow one tool call from dispatch to result, filter
on its `tool_use_id`.

A line that is not valid JSON, such as a panic backtrace from a dependency, is
kept as written and treated as an error, so a level filter never hides it.

## Level

The default level is `info`. Set `storage.log_level` in `caudra.toml` to
`trace`, `debug`, `info`, `warn`, or `error`.

```toml
[storage]
log_level = "debug"
```

`RUST_LOG` overrides the config for one run, and takes the full
[`tracing` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html),
so you can raise one target without raising the rest:

```bash
RUST_LOG=caudra::provider=debug caudra
```

`RUST_LOG` never affects telemetry export, which has its own filter. See
[Telemetry](/docs/telemetry/).

## Privacy

Prompt text, model output, and tool input are not written. A prompt is recorded
as `prompt_length` and a tool call as its name, its source, and how long it took.

Provider error messages are written in full, because they are usually the only
thing that explains a failed run and they stay on your machine. A provider that
echoes the request back in an error body puts that body in the log, so read the
file before pasting it into an issue. The collector only ever receives the status
code.

The opt-ins live under `telemetry`: `log_user_prompts` adds the prompt and
`log_tool_details` adds the tool input, to the log file and to the collector
together. Both require `telemetry.enabled`, so with telemetry off the text is
never written anywhere. Turn them on while debugging and off afterwards.
[Telemetry](/docs/telemetry/#what-does-not) lists what each one adds.

## Writing

The writer runs on its own thread and the agent never waits for it. When a burst
outruns the disk, the newest records are dropped and a warning records how many,
which keeps a slow disk from slowing a run. Caudra flushes the queue on a clean
exit and on a panic, so the record that explains a crash reaches the file.

Lua plugins write to the same file through `caudra.log.info|warn|error`. They
run only when `experimental.lua_plugins` is on. See
[Plugins](/docs/plugins/#development-loop).
