---
title: "Secrets"
description: "Give shell commands an API token or password without pasting it into chat, and manage what each session keeps."
---

When a command needs a credential, such as `wrangler` needing a Cloudflare token, the agent asks for it on a private card. You type or paste the value there. It never enters the chat, and the session's shell commands receive it as an environment variable.

## The card

The agent calls the `request_secret` tool with a variable name, such as `CLOUDFLARE_API_TOKEN`, a title, and a line that says what the secret is for and where to get it. The card shows these above a masked field. The value never appears on screen, and copying the card or selecting it with the mouse leaves the value out.

| Choice | Effect |
|---|---|
| `Save` (`Enter`) | This session's shell commands receive the value |
| `Remember for all sessions` | Every session's shell commands receive it, until you delete it |
| `Decline` (`F3`) | Caudra tells the agent you declined and saves nothing |

If the name already holds a secret, the card says that saving replaces it. The model reads only the variable's name and whether you saved it.

A name must be a valid variable name. Caudra refuses names that decide how Bash starts, such as `BASH_ENV`, and names that shell commands already get from Caudra, such as `PATH` and `HOME`, before any card appears.

## How commands receive a secret

Every shell command that the session or one of its subagents runs on this machine gets the secret as `$NAME`. So do the commands that [pstack's scripts](/docs/code/#pstack-scripts) run. The agent writes `$CLOUDFLARE_API_TOKEN` in the command, and Bash expands it. A session secret wins over an `[agent.shell_env]` value with the same name.

When a command prints the value, Caudra replaces it with `${NAME}` before it shows the output, saves it, or sends it to the model. This applies to the final output, to live output while the command runs, and to the output of a cancelled command.

The `execution_environment` tool lists `NAME (session secret)` among the forwarded variables, so the agent can tell which secrets are set after you resume or edit them.

## Session and remembered secrets

A session secret lasts 24 hours from when it was last saved. It survives quitting and resuming. After 24 hours shell commands stop receiving it, and `/env` marks it `expired`. Caudra keeps it in an owner-only file under `sessions/<id>/secrets/` in the [state directory](/docs/configuration/#directory-layout), and deleting the session deletes it.

A remembered secret is an owner-only file, `secrets/<NAME>` in the state directory. Caudra adds an `[agent.shell_env] set` entry to the global `caudra.toml` that reads it:

```toml
[agent.shell_env]
set = { CLOUDFLARE_API_TOKEN = "{file:/home/me/.local/state/caudra/secrets/CLOUDFLARE_API_TOKEN}" }
```

It reaches every local session from the next shell command, as any [forwarded variable](/docs/cli/#forwarding-more-variables) does. If `set` already holds a different value for that name, Caudra saves the secret for this session only and tells the agent why. You can then move it in `/secrets`, which asks before it replaces the entry.

## Manage secrets

`/secrets` opens [`/env`](/docs/cli/#forwarding-more-variables) at its Secrets section. Each row shows a name, a `session` or `remembered` badge, and how long a session secret has left. Values are always masked, and `r` cannot reveal them.

| Key | Action |
|---|---|
| `s` | Save a new secret, for this session |
| `e` | Replace the selected secret. The field starts empty, because Caudra never shows the old value |
| `m` | Move the selected secret between this session and all sessions |
| `d` | Delete the selected secret, after you confirm |

A replaced or moved secret applies from the next shell command. When you replace a remembered secret, every session in this Caudra process reads the new file at once. Another running Caudra process reads it the next time it loads its config. Deleting a remembered secret removes the `set` entry first and then the file, so the config never points at a missing file.

## Where secrets work

Secrets reach only shell commands that run on this machine.

- In a [remote Workcell](/docs/remote-workspaces/) or a [sandbox](/docs/sandboxes/) session, Caudra refuses the agent's request, and the Secrets section of `/env` holds only a note that says so.
- [Headless runs](/docs/headless/), such as `caudra -p`, have no card to show, so Caudra refuses the request.
- Over [ACP](/docs/acp/#permissions-and-questions), a client that supports form elicitation gets a form with the value field marked `caudra/masked` in its `_meta`, and a choice between this session and all sessions. A client that reads the mark masks the field. A client without form elicitation gets a refusal. `/secrets` is available only in the terminal UI.
- [Autopilot](/docs/permissions/#autopilot) never answers the card for you.

## What this protects

The value stays out of the chat, the transcript, tool results, saved tool outputs, the session database, logs, telemetry, and every request to the model provider.

It does not hide the secret from the agent itself. An agent that can run commands can print the value in a form Caudra does not recognise, write it to a file, or send it over the network. Redaction only catches the exact value, so a command that encodes, splits, or transforms it shows the result as is. Approve commands that use a secret with the same care as any other, and use a token with the narrowest scope that does the job.

The files on disk are readable only by you and are not encrypted.
