---
title: "Notifications"
description: "Know when a session finishes or needs your input."
---

Caudra can tell you when a session finishes or needs your input. This is useful
when you move to another terminal while Caudra works.

Notifications are enabled by default. In the interactive TUI, `auto` uses
persistent pane status when the immediate terminal supports OSC 7501. Otherwise,
it uses the legacy notifications described below. Native Herdr reporting takes
precedence. This does not change headless or ACP behavior.

## Configuration

Set `ui.notifications` in `~/.config/caudra/caudra.toml`:

```toml
[ui]
notifications = "auto"
```

| Value | Behavior |
| --- | --- |
| `auto` | Prefer native Herdr reporting, then OSC 7501 status when supported. Otherwise use OSC 9 in a supported terminal or BEL. |
| `osc9` | Always send an OSC 9 notification. |
| `bell` | Always send the terminal bell. |
| `off` | Do not send notifications. |

Explicit `osc9`, `bell`, and `off` settings do not probe for or use OSC 7501.

## Persistent pane status

Outside native Herdr reporting, the interactive TUI with `auto` probes its
immediate terminal once for OSC 7501 support. A supporting receiver gets
persistent `idle`, `working`, `blocked`, `done`, or `error` status. A blocked
status can identify the kind as `permission`, `question`, or `auth`. The receiver
decides how to display status and whether to notify you. OSC 7501 does not
guarantee a desktop notification.

Caudra reports one aggregate status for the pane, without per-session or task
trees. Priority is `blocked` > `working` > unacknowledged `error` > unacknowledged
`done` > `idle`. Cancellation returns that session to idle after its remaining
work settles. Completed outcomes are acknowledged when you interact with the
Caudra session that owns them. Focus alone does not acknowledge them.

Status messages are fixed, generic text. They contain no prompt or response
content, tool arguments, or error details. This privacy guarantee applies to
OSC 7501 status, not the response previews in legacy OSC 9 notifications.

A terminal multiplexer must itself support OSC 7501. Caudra does not forward
status to the outer terminal through DCS passthrough. The tmux and GNU screen
instructions below apply only to legacy notifications.

## Legacy notifications

When OSC 7501 is unsupported, `auto` uses OSC 9 for Ghostty, iTerm2, Kitty, Warp,
and WezTerm. An unknown terminal uses BEL. Your terminal settings decide whether
BEL makes a sound or shows a visual alert.

Caudra also recognizes `xterm-ghostty` and `xterm-kitty` from `TERM`. This lets
OSC 9 work when an SSH connection does not preserve `TERM_PROGRAM`.

Caudra suppresses legacy notifications while it knows that its terminal has
focus. OSC 9 uses these messages:

- `Agent turn complete` or a preview of the response, up to 200 characters.
- `Permission requested: <tool>` for a permission prompt.
- `Authentication required` when authentication needs attention.
- `Question requested` for a question prompt.
- `Plan ready` when a plan is ready.
- `automation <name>: <text>` when an [automation](/docs/automations/#actions)
  calls `notify()`, with the text cut to 200 characters. The status bar flashes
  it too.

Automation `notify()` remains an explicit notification through this route,
even when OSC 7501 status is active. It does not become a pane status update.

Response previews and automation notices can appear in your operating system's
notification history. Caudra does not include tool arguments, permission scopes,
question bodies, plan content, or error details. Use `bell` for a message-free
alert, or use `off` to disable notifications if that text should not reach
notification history.

## tmux

For legacy notifications, tmux needs both of these settings:

```tmux
set -g focus-events on
set -g allow-passthrough all
```

Use `allow-passthrough all`, not `allow-passthrough on`. The `on` value permits
passthrough only while the Caudra pane is visible. tmux drops the notification
after you change to another tmux window.

Add the settings to `~/.tmux.conf`, then reload the file or restart tmux.

## Other terminal multiplexers

Caudra wraps OSC 9 for GNU screen. GNU screen does not pass terminal focus
events to Caudra, so Caudra does not suppress notifications there. A notification
can appear while the GNU screen window has focus.

Caudra sends OSC 9 directly through Zellij.

## Herdr

When native Herdr reporting is active, it remains authoritative. `auto` sends
no separate notifications and does not probe for or use OSC 7501. Caudra reports
its state to Herdr, and Herdr shows its own notice when the agent finishes or
waits for you. The notice names the prompt that is waiting, such as
`Permission requested: shell`. An explicit `osc9` or `bell` setting still
applies.

Remote Caudra over SSH without native Herdr reporting can use OSC 7501 if its
immediate receiver supports it. See [Worktrees](/docs/worktrees/#herdr-integration)
for what else Caudra reports through the native integration.

## Focus on Windows

Terminal focus reporting is not available on Windows. Caudra treats the
terminal as unfocused so an explicit `bell` or `osc9` setting still works.
OSC 7501 detection is currently Unix-only. On Windows, `auto` uses the legacy
notification fallback.
