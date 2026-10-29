---
title: "Continual learning"
description: "How Caudra learns from your conversations in the background, where it writes what it learned, and how to review, revert, or turn it off."
---

Caudra learns from how you work with it. When you correct the agent, cut an answer off, refuse a call, or repeat a preference, a background learner notices. It writes what will still hold next month into your instruction files and the project's [memory](/docs/memory/), so later sessions start out knowing it.

The learner never takes a turn of your conversation, and nothing it reads or writes enters the context of the session it runs beside. Running sessions see an instruction file change as the usual [instructions changed](/docs/context/#instruction-files) notice, and a new note as a memory update.

## When it runs

Each interactive session (the TUI, the SDK and ACP) runs a learner for its project. It runs when all of these hold:

- At least 10 messages you typed arrived since the last run, across every session in the project. Messages from automations, autopilot, and other sessions do not count.
- At least 2 hours passed since the last run. When the last run had more history than it could read, the next one can start after 10 minutes.
- The session has been idle for a minute, so the learner does not compete with a turn for rate limits.

While a project has no [memory notes](/docs/memory/), its first 2 runs of the day need only 3 typed messages and 10 minutes since the last run. Later sessions then start with notes to build on.

A project runs at most 8 times a day. A lease in the state database keeps two Caudra processes from learning the same project at once. `--print` runs, ephemeral sessions, and remote workspaces never learn.

The first run in a project reads the last 7 days. Each later run reads from where the last one stopped.

## What it reads

The learner starts from a digest of the project's conversations since its last run: every message you typed, the answer it replied to, and whether you cut that answer off, refused a call, or a tool failed. The digest is redacted before the model sees it and holds at most 48 KiB. Each exchange carries a history reference.

The learner is a read-only subagent on the **Learn** model job. It checks each candidate against the project's history with `history.search` and `history.read` in [`code`](/docs/code/), and against what memory and the instruction files already say. Its history access is held to the current project, so a fact from another project can never reach a committed file. Its own conversation is kept in memory only, and later runs never read it.

A run stops, and changes nothing, after 40 tool calls, 300,000 tokens, or 20 minutes.

## Where it writes

The learner reports its findings, and Caudra checks every item before it writes anything. The model never writes a file.

| Target | File | What goes there |
| --- | --- | --- |
| `project` | The project's instruction file, `AGENTS.md` unless another one is already loaded | Conventions and facts true for anyone working on the project |
| `local` | `AGENTS.local.md` beside it | Facts about your machine or setup in this project |
| `global` | `~/.config/caudra/AGENTS.md` | Your own preferences, in every project |
| `memory` | Project memory notes | Detail too long or narrow for an always-loaded file |

In each instruction file, the learner owns one block between two markers. It never edits text outside them:

```markdown
<!-- caudra:learned -->
## Learned workspace facts

- Run tests with cargo nextest, never cargo test
<!-- /caudra:learned -->
```

Caudra applies these rules:

- A block holds at most 12 bullets. Each is one line of at most 240 bytes.
- Every new or changed bullet and every note cites history references from this project.
- A bullet in the committed project file needs evidence from at least two sessions.
- Text that looks like a secret, and text that holds a block marker, is refused.
- A note is at most 4 KiB.
- A file is created only when the learner has something to write to it.
- Before it writes `AGENTS.local.md`, Caudra adds the file to the `.gitignore` beside it, creating `.gitignore` if there is none, even outside a git repository. It skips this when git already ignores the file. The line stays when you revert a change, and `/learn` doesn't list it.

An item that breaks a rule is dropped with its reason, and the rest still apply. Before each write Caudra compares the file with the copy the learner was shown. A file you changed meanwhile is left alone, and that history is read again on the next run.

Memory notes the learner writes are journaled with the origin `learner:<run>`, so the [`/memory` inspector](/docs/memory/#the-inspector) shows where they came from.

## Reviewing what it learned

When a run changes something, the status bar says so briefly. SDK clients receive a `learning_notice` system message.

`/learn` lists the project's runs, newest first, with what each one changed and what it dropped and why.

| Key | Action |
| --- | --- |
| `↑` / `↓` | Select a run, which opens under its row |
| `←` / `→` | Select a change in that run |
| `t` / `Enter` | Open the selected run's transcript |
| `n` | Run now, ignoring the message count and the spacing between runs |
| `u` | Revert the selected change |
| `p` | Pause or resume learning for this project |

A revert restores a block or note only while it still holds what the run wrote. A note the run created is deleted. Changes to the committed project file also show in git and the [workbench](/docs/workbench/), so you can review them before you commit.

The transcript shows the learner's conversation: the digest it was given, its `history` calls, and its report. A run in progress in this session updates live. Drag over the transcript to select a passage, and releasing the mouse button copies it. `Esc` goes back to the list. Caudra keeps the transcripts of each project's last 20 runs, compressed, so finished runs open from any session. Transcripts are never indexed for `history.search`.

## Configuration

Learning is on by default. Turn it off, or narrow where it writes, in `caudra.toml`:

```toml
[agent]
continual_learning = false
learning_targets = ["local", "global", "memory"]
```

A project's `.caudra/caudra.toml` can turn learning off for that project or narrow its targets, and never turn it on. See [Configuration](/docs/configuration/) for every key.

The learner runs on the **Learn** model job, which follows your chat model until you bind it to another in `/model`. Its spend is recorded under the `learning` purpose in `/usage`. See [Model jobs](/docs/providers/#model-jobs).
