---
title: "Notifications"
description: "Know when a session finishes or needs your input."
---

Caudra can tell you when a session finishes or needs your input. This is useful
when you move to another terminal while Caudra works.

Notifications are enabled by default. In the interactive TUI, `auto` uses
persistent pane status when the immediate terminal supports OSC 7501. Otherwise,
it uses the legacy notifications described below. Native [Herdr](#herdr) and
[Orca](#orca) reporting take precedence. This does not change headless or ACP
behavior.

## Configuration

Set `ui.notifications` in `~/.config/caudra/caudra.toml`:

```toml
[ui]
notifications = "auto"
```

| Value | Behavior |
| --- | --- |
| `auto` | Prefer native Herdr or Orca reporting, then OSC 7501 status when supported. Otherwise use OSC 9 in a supported terminal or BEL. Orca also gets BEL, as [Orca](#orca) describes. |
| `osc9` | Always send an OSC 9 notification. |
| `bell` | Always send the terminal bell. |
| `off` | Do not send notifications. |

Explicit `osc9`, `bell`, and `off` settings do not probe for or use OSC 7501.
Native Herdr and Orca status keeps working with every setting, including `off`.

## Persistent pane status

Outside Herdr and Orca, the interactive TUI with `auto` probes its
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

`blocked` means you must act: a permission, question, plan approval, or held
[peer messages](/docs/messaging/#find-peers-and-review-messages) waiting for you. An open login picker is
`blocked` with kind `auth`. While Caudra waits on its own work, such as saving
a permission choice, acknowledging a sandbox, or transferring files, the status
is `working`. Herdr and Orca read the same status.

While [autopilot](/docs/autopilot/) decides a prompt, question, or plan for
you, the pane reads as `working`. When autopilot leaves one to you, the pane
reads as `blocked`, with the kind `permission` for a permission prompt and
`question` for a question or a plan. The end of autopilot remains an explicit
notification, as automation `notify()` does.

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
- `Autopilot needs you: <kind>` when [autopilot](/docs/autopilot/) leaves a
  permission prompt, question, or plan to you. `<kind>` is `permission`,
  `question`, or `plan`. While autopilot is still deciding, nothing is sent.
- `Autopilot ended: <reason>` when autopilot ends, such as `goal met` or
  `continuation limit reached`. An end caused by an error reads as `error`.

Automation `notify()` and the end of autopilot remain explicit notifications
through this route, even when OSC 7501, Herdr, or Orca status is active. They do
not become pane status updates. With `auto`, they use OSC 9 or BEL as described
above.

Response previews and automation notices can appear in your operating system's
notification history. Caudra does not include tool arguments, permission scopes,
question bodies, plan content, autopilot's reasons, or error details. Use `bell`
for a message-free alert, or use `off` to disable notifications if that text
should not reach notification history.

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
no separate notices for turns and prompts, and it does not probe for or use
OSC 7501. Caudra reports its state to Herdr, and Herdr shows its own notice when
the agent finishes or waits for you. The notice names the prompt that is
waiting, such as `Permission requested: shell`. Held peer messages show as
`Peer messages await approval`. A finished or failed turn shows as idle, since
Herdr has no separate done state. An explicit `osc9` or `bell` setting still
applies.

Remote Caudra over SSH without native Herdr reporting can use OSC 7501 if its
immediate receiver supports it. See [Worktrees](/docs/worktrees/#herdr-integration)
for what else Caudra reports through the native integration.

## Orca

Inside an [Orca](https://github.com/stablyai/orca) terminal, Caudra reports its
status to Orca as OSC 9999, the sequence Orca reads from agent output. Orca shows
the agent state and raises its own notice when the agent finishes or waits for
you. `auto` does not probe for OSC 7501 there.

In a remote workspace, Orca stops reading the status of a hidden tab after a
while, so the tab keeps its old state, such as `working`. With `auto`, Caudra
therefore also rings the bell when a turn ends or a prompt waits while its
terminal does not have focus. Orca then marks the tab unread. For a desktop
notice as well, turn on **Terminal Bell** in Orca's notification settings. When
the terminal gets focus again, Caudra sends its status again, so the tab shows
the current state. An explicit `osc9` or `bell` setting still applies, and `off`
stops the bell.

Each report carries the state, the model, and the label of the prompt that
blocks the agent, such as `Permission requested: shell`. Once the session is
saved, it also carries the session id as `sessionId`, which Orca resumes the
session with as `caudra --resume <id>`. While subagents run, it lists each by
its task id and description, which Orca shows as child rows under the agent in
its sidebar. It never carries the session title,
prompt or response text, tool arguments, or error details. Orca
still sees the session title as the window title, which it shows on the tab, as
[Tab names](/docs/sessions/#tab-names) describes. Orca has no field for context
usage. Starting idle and exiting on purpose report a session boundary, so Orca
does not announce a finished task.

Off Windows, Caudra also marks its status and each subagent as `openable`.
Clicking a subagent's child row then switches Caudra to that subagent's chat,
and clicking the agent's own row switches back to the main chat. Orca writes
`OSC 9999 ; {"openSubagent":"<task id>"} BEL` or
`OSC 9999 ; {"openMain":{}} BEL` into the terminal's input, and Caudra reads it
as a request rather than typing. Caudra ignores any other request it receives
this way, and one for a task it does not know. The Windows
console turns such a sequence into keystrokes, so there a click only focuses
the terminal. An Orca that does not support this also only focuses the
terminal.

Caudra wraps OSC 9999 for tmux and GNU screen. Inside tmux, set
`allow-passthrough on` or `all`. Zellij drops the sequence, so Caudra sends no
status to Orca when it runs under Zellij. Herdr running inside an Orca terminal
counts as Herdr.

## Focus on Windows

Terminal focus reporting is not available on Windows. Caudra treats the
terminal as unfocused so an explicit `bell` or `osc9` setting still works.
OSC 7501 detection is currently Unix-only. On Windows, `auto` uses the legacy
notification fallback.
