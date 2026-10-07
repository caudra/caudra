+++
title = "Notifications"
weight = 9
[extra]
group = "Reference"
+++

# Notifications

Caudra can tell you when a session finishes or needs your input. This is useful
when you move to another terminal while Caudra works.

Notifications are enabled by default. Caudra does not notify while it knows
that its terminal has focus.

Caudra uses these messages:

- `Agent turn complete` or a preview of the response, up to 200 characters.
- `Permission requested: <tool>` for a permission prompt.
- `Authentication required` when authentication needs attention.
- `Question requested` for a question prompt.
- `Plan ready` when a plan is ready.
- `automation <name>: <text>` when an [automation](/docs/automations/#actions)
  calls `notify()`, with the text cut to 200 characters. The status bar flashes
  it too.

Response previews and automation notices can appear in your operating system's
notification history. Caudra does not include tool arguments, permission scopes,
question bodies, plan content, or error details. Use `bell` for a message-free
alert, or use `off` to disable notifications if that text should not reach
notification history.

## Configuration

Set `ui.notifications` in `~/.config/caudra/caudra.toml`:

```toml
[ui]
notifications = "auto"
```

| Value | Behavior |
| --- | --- |
| `auto` | Use OSC 9 in a supported terminal. Use BEL otherwise. |
| `osc9` | Always send an OSC 9 notification. |
| `bell` | Always send the terminal bell. |
| `off` | Do not send notifications. |

`auto` supports Ghostty, iTerm2, Kitty, Warp, and WezTerm. An unknown terminal
uses BEL. Your terminal settings decide whether BEL makes a sound or shows a
visual alert.

Caudra also recognizes `xterm-ghostty` and `xterm-kitty` from `TERM`. This lets
OSC 9 work when an SSH connection does not preserve `TERM_PROGRAM`.

## tmux

tmux needs both of these settings:

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

In a Herdr pane, `auto` sends no notifications. Caudra reports its state to
Herdr, and Herdr shows its own notice when the agent finishes or waits for you,
so a second one from Caudra would repeat it. The notice names the prompt that
is waiting, such as `Permission requested: shell`. An explicit `osc9` or `bell`
setting still applies. See [Worktrees](/docs/worktrees/#herdr-integration) for
what else Caudra reports to Herdr.

## Focus on Windows

This terminal focus protocol is not available on Windows. Caudra treats the
terminal as unfocused so an explicit `bell` or `osc9` setting still works.
