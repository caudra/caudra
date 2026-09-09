+++
title = "Sessions, Forks, and Revert"
weight = 35
[extra]
group = "Guides"
+++

# Sessions, forks, and revert

Caudra stores conversation history as parent-linked items. User prompts, assistant text, reasoning, tool calls, and tool results are separate items. A session head selects the active path through those items. Moving the head keeps the abandoned path available for unrevert and later forks.

## Active sessions

One Caudra runtime can own a session ID at a time. Opening that session from
another Caudra process fails before the agent starts or executes tools. Sessions
with different IDs can run together, including background tabs in one TUI.

Caudra releases ownership on normal exit, process termination, or a crash. A
running process that has stopped responding still owns its session. Forking an
active session remains available because the child receives a new ID.

## Ephemeral sessions

Run `caudra --ephemeral` for a session that leaves no session record behind. Set `storage.ephemeral = true` to make this the default.

Caudra creates a private temporary state root under `XDG_RUNTIME_DIR` or the system temporary directory. Session rows, tool outputs, snapshots, input history, and stashed prompts use that root. It is removed when Caudra exits through its normal success or error paths. A forced process kill can leave the temporary root for the operating system to clean up.

Credentials, configuration, trust, model preferences, plans, memory notes, and logs keep their normal persistent locations. Project and global permission decisions remain durable. Ephemeral mode starts with an empty session store, so saved sessions and the persisted tab layout are unavailable during that run.

What the run spends is still recorded in the persistent [usage ledger](/docs/token-economy/#lifetime-spend), labelled as ephemeral, so an ephemeral run stays visible in your spending totals.

## Titles

A new session is named from its first prompt right away, trimmed to 100 characters at a word boundary, so it is findable in the session picker before the first reply arrives.

While the turn runs, Caudra asks a small model for a better name and replaces the trimmed text in place. That request is detached from the turn: it never delays a reply, and a failure or timeout leaves the trimmed title standing. It happens once per session, on the first prompt only.

A title you set yourself is never overwritten. Renaming through the session picker, the Lua API, or a fork marks the title as yours, and a generated name arriving afterwards is dropped.

Choose which model writes titles under the Title purpose in `/model`, described in [Providers](/docs/providers/). Set `agent.generate_titles = false` to keep the trimmed prompt and skip the request.

## Message actions

Click `⋮` in the gutter beside a message to open Message Actions. Right-clicking the message also works when the terminal forwards it. Clicks in the message body still select text, expand reasoning, and interact with tool output.

The menu offers:

- **Fork here** creates and focuses a new session.
- **Revert both** moves the conversation head and restores files.
- **Revert conversation** moves only the conversation head.
- **Revert files** restores only the workspace.
- **Unrevert** restores the head and workspace captured before the last revert.

Revert and unrevert require every live session in the workspace to be idle. Active shell commands and subagents also block them. Forking never starts a model request.

## Fork boundaries

| Selected item | New session history | Composer |
|---|---|---|
| User prompt | Stops before the prompt | Prompt text and images restored for editing |
| Assistant text | Includes the selected text item | Empty |
| Reasoning | Includes the selected reasoning item | Empty |
| Completed tool | Includes the call and result | Empty |
| Incomplete tool call | Includes the call; history repair supplies an unavailable result | Empty |

The child receives a new session ID and a title such as `Original title (fork #1)`. It copies the selected ancestor path, reachable tool outputs and subagent histories, model and execution settings, and snapshots for that path. Usage totals, goals, queues, pending revert state, conversation permission rules, and explicit YOLO state start clean.

Subtasks are different from user-created forks. They share the root conversation's permission rules. Resuming that root restores its rules, while `/new` starts a clean root.

Forking does not restore files. The child uses the same working directory and sees its current contents. Use a revert action first when the workspace must match an older conversation point.

## Managed tool outputs

Retained tool output belongs to one session and is stored under `tool-output/<session-id>/` in the Caudra state directory. An output ID can be read or searched only from its owning session. Retained outputs expire only through [retention](#retention).

Each retained output is capped at 100 MiB. Deleting a session deletes its retained outputs. A fork copies the outputs referenced by its selected ancestor path and reachable subagent histories into the child session, preserving their opaque IDs there.

If a failed deletion or interrupted write leaves output without a session, output-store startup cleanup removes it after a seven-day grace period. Retained outputs belonging to a live session remain untouched.

## Retention

Sessions have two tiers. A **full** session keeps everything: the conversation, rich tool output records, retained tool output files, rewind archives, and file snapshots. A **transcript** session keeps the conversation, subagent transcripts, usage, model, mode, drafts, queue, and permission rules, and can still be resumed. It has no file revert, no `tool_output_read` access to old outputs, and renders old tool calls from their model-facing text. Small structured records such as todo lists stay.

Trimming moves a session from full to transcript. Forgetting deletes it. A trimmed session that runs again becomes full for its new work and is trimmed again later.

Policies use the vocabulary of `restic forget`. A session is kept when any rule matches:

| Rule | Keeps |
|------|-------|
| `keep_last = N` | the N most recently active sessions |
| `keep_hourly`, `keep_daily`, `keep_weekly`, `keep_monthly`, `keep_yearly = N` | for the last N periods that contain sessions, the newest session of each |
| `keep_within = "90d"` | every session active within the duration |
| `keep_within_hourly` ... `keep_within_yearly = "7d"` | one session per period within the duration |

Activity is the later of the last write and the last open. Calendar rules use natural boundaries in the local time zone: hours on the hour, days at midnight, ISO weeks from Monday. Durations are a sequence of `y`, `m`, `d`, and `h` parts, for example `2y5m7d3h`. `w` is not accepted, write `7d`.

The policy is evaluated per working directory by default, so one busy project cannot starve another project of its kept sessions. Pinned sessions, sessions open in any Caudra process, sessions with a pending revert, and sessions with activity in the future are always kept.

The default keeps the twenty most recently active sessions of every directory in full, trims anything older than ninety days, and never forgets:

```lua
caudra.setup({
    storage = {
        retention = {
            group_by = "directory",
            sweep_interval_hours = 24,
            trim = { keep_last = 20, keep_within = "90d" },
            forget = {},
        },
    },
})
```

An empty `forget` policy disables automatic deletion. To delete sessions after two years, set `forget = { keep_within = "2y" }`. Set `sweep_interval_hours = 0` to run retention only through the CLI.

The sweep runs on a background thread once per interval while the TUI is open. It trims, forgets, and then prunes: due cleanup jobs run, orphaned artifact directories older than seven days are removed, the write-ahead log is checkpointed, and free pages are returned to the filesystem. Every step is transactional or idempotent, so an interrupted sweep leaves nothing inconsistent.

`caudra storage trim --dry-run` and `caudra storage forget --dry-run` print the plan with the reason each session is kept. `caudra storage sessions` lists sessions with their tier. `caudra storage pin <ID>` keeps a session regardless of policy. See [CLI](/docs/cli/#caudra-storage) for every flag.

Retention never removes spending records. What a session cost is written to a separate ledger that no session owns, so `caudra storage usage` still answers after the sessions are gone. See [Lifetime spend](/docs/token-economy/#lifetime-spend).

## What the state directory holds

`/storage` shows where the disk went. A proportional bar splits the state directory between the session database, tool output files, workspace snapshots, and archives, and a legend gives each one its exact size. Below it, the database section reports file and write-ahead log sizes, free pages that a prune would reclaim, and the session and item counts behind them.

The snapshot section lists workspace stores largest first with their size, object count, and number of snapshots. A store whose workspace root is gone is marked rather than hidden, because an orphaned store is usually the one worth deleting. `/storage all` lists every store instead of the largest few; the footer command toggles between them.

Measuring walks the snapshot stores on disk, so the modal opens immediately and fills in when the walk finishes. The same figures are available without the TUI from `caudra storage stats` and `caudra storage snapshots`.

To reclaim one store the modal named, trim that session: `caudra storage trim <ID>`. Trimming drops the workspace snapshots, retained tool output files, and rewind archives while the conversation stays and stays resumable, so it is the right answer when a single session has grown out of proportion and you still want its transcript. Pinned sessions are refused and a session open in another process is skipped, so the command is safe to run while Caudra is up. Add `--dry-run` to see the session and its artifact size first.

## Conversation revert

A conversation revert changes the active head rather than deleting items. Selecting a user prompt lands before that prompt and restores it to the composer. Other item types are inclusive.

Sending a prompt after revert creates a new branch from that point. The abandoned branch remains stored. Unrevert is available until new work commits the new branch.

`Esc Esc` while idle uses the same conversation-only mechanism through the rewind picker.

## File snapshots

Caudra creates a session-start snapshot before the first top-level run. It snapshots the current history head before each later run and the resulting head after completion or cancellation. Reusing a head refreshes its pre-run snapshot, so edits made while idle are included. A file restore selects the nearest available snapshot at or before the chosen item. Several parallel tool calls therefore share one safe run checkpoint.

Snapshots are content-addressed with SHA-256 and stored under the Caudra state directory in `session-snapshots/<session-id>/<workspace-hash>/`. The object store contains the complete file bytes under their hashes. A checkpoint manifest maps each relative path to its object hash and Unix mode. Unchanged files reuse the same object instead of storing another copy.

Each session workspace has a 512 MiB retention target. Old checkpoint manifests are removed first. The session-start anchor and data needed by an active revert remain available, so protected data can exceed the target.

In a Git worktree, snapshot walks follow Git ignore rules. Outside Git, Caudra walks all regular files below the session directory. `.git`, symlinks, special files, and paths outside the session directory are not captured. Changing a path between captured and ignored or symlink state is outside the restore guarantee because manifests cannot distinguish that state from absence.

Restore compares the current file hash and mode with the source snapshot. If a tracked path changed outside the captured run, restore aborts and reports a conflict. Caudra does not overwrite it automatically. Conversation-only revert remains available when file restore cannot proceed.

Only paths that differ between the source and target manifests are touched. Files created after the target are deleted, deleted files are recreated, and unrelated files remain in place. Each file replacement is atomic. A restore spanning several files completes through the journal.

Before applying changes, Caudra captures the current state of every affected path. Unrevert restores this state before moving the conversation head back. A restore journal records prepare, apply, and verification phases so a later restore can finish an interrupted transaction.

Snapshots cover regular files. They cannot reverse running processes, databases, network calls, Git branches or index state, nested repository state, or commands that changed files outside the session directory. Changes from manual shell commands can appear as conflicts. `/cd` changes the process workspace for every live session and is blocked while any session is busy or has a pending revert.
