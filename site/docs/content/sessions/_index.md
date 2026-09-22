+++
title = "Sessions, Forks, and Revert"
weight = 35
[extra]
group = "Guides"
+++

# Sessions, forks, and revert

Caudra stores conversation history as parent-linked items. User prompts, assistant text, reasoning, tool calls, and tool results are separate items. A session head selects the active path through those items. Moving the head keeps the abandoned path available for unrevert and later forks.

For remote sessions, transcripts stay on the client and workspace snapshots stay on the Workcell server. Resume requires the original remote workspace identity and generation. See [Remote session identity and recovery](/docs/remote-workspaces/#cwd-and-resume) before moving an endpoint, replacing a workspace, or recovering an interrupted mutation. The `storage` subcommand is disabled when a remote Workcell selector is supplied.

Managed sandbox sessions retain their sandbox source. `caudra --session ID` resolves it before validating the remote workspace, and a paused VM requires explicit resume approval. See [sandbox conversation and VM resume](/docs/sandboxes/#resume-a-conversation-or-vm). Saving or deleting conversation history does not delete the VM or extend its disk-retention deadline.

## Active sessions

One Caudra runtime can own a session ID at a time. Opening that session from
another Caudra process fails before the agent starts or executes tools. Sessions
with different IDs can run together, including background tabs in one TUI.

Caudra releases ownership on normal exit, process termination, or a crash. A
running process that has stopped responding still owns its session. Forking an
active session remains available because the child receives a new ID.

A session is recorded once it holds something worth keeping, such as a prompt, a
typed draft, or a queued message. Starting Caudra and quitting leaves no session
behind, so the picker and `caudra --continue` skip it.

## Moving sessions to another directory

Use `/migrate-sessions` to move every saved local session with one exact stored working directory. The source directory may already be gone. Choose a destination directory and review the confirmation before applying the move.

The bulk confirmation checks **Include historical project usage** by default. Toggle it off to leave lifetime project attribution unchanged. When included, the move reattributes all usage recorded for that exact source directory, including deleted sessions and ephemeral runs. It covers every model, purpose, and payer category, but excludes child directories with a different recorded cwd. Overlapping destination buckets are added together without replacing existing destination spend. The completion message reports usage buckets moved and merged, or an empty source ledger, separately from sessions moved.

A single-session move retains that session's own counters but leaves the shared lifetime usage ledger attributed to its recorded project. The ledger cannot identify one session's contribution, so single-session moves do not offer the historical usage option.

Before a whole-project rename, stop other Caudra processes using the source directory, including ephemeral runs. Caudra drains the invoking runtime's usage writes before committing the move and aborts if that drain fails. Only ledger rows present at the relocation transaction are reattributed. There is no permanent redirect, so new work started at the old directory records usage there again.

Session IDs and conversations are preserved. Active source plans and approvals are detached. Files and old workspace snapshots are not moved. If moving live tabs requires a project environment reload, Caudra exits after committing and asks you to run `caudra --continue` from the destination.

## Ephemeral sessions

Run `caudra --ephemeral` for a session that leaves no session record behind. Set `storage.ephemeral = true` to make this the default.

Caudra creates a private temporary state root under `XDG_RUNTIME_DIR` or the [scratch directory](/docs/configuration/#directory-layout). Session rows, tool outputs, snapshots, input history, and stashed prompts use that root. It is removed when Caudra exits through its normal success or error paths. A forced process kill can leave the temporary root for the operating system to clean up.

Credentials, configuration, trust, model preferences, plans, memory notes, and logs keep their normal persistent locations. Project and global permission decisions remain durable. Ephemeral mode starts with an empty session store, so saved sessions and the persisted tab layout are unavailable during that run.

Managed sandbox lifecycle records and transfer recovery journals also remain persistent. `--ephemeral` is not a disposable-VM or automatic-delete policy.

What the run spends is still recorded in the persistent [usage ledger](/docs/token-economy/#lifetime-spend), labelled as ephemeral, so an ephemeral run stays visible in your spending totals.

## Titles

A new session is named from its first prompt right away, trimmed to 100 characters at a word boundary, so it is findable in the session picker before the first reply arrives.

While the turn runs, the Title job asks its resolved model for a better name and replaces the trimmed text in place. Title follows Fast when unbound. That request is detached from the turn: it never delays a reply, and a failure or timeout leaves the trimmed title standing. It happens once per session, on the first prompt only.

A title you set yourself is never overwritten. Renaming through the session picker, the Lua API, or a fork marks the title as yours, and a generated name arriving afterwards is dropped.

Choose its assignment from the Title row in `/model`, described in [Providers](/docs/providers/#model-jobs). Set `agent.generate_titles = false` to keep the trimmed prompt and skip the request.

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

## Size ceiling

Opening a session hydrates all of it at once: the conversation, every rich tool output record, and every subagent transcript. `storage.max_eager_load_mb` caps what Caudra will hydrate that way, at 1024 MB by default. A session above the cap refuses to open and names both ways out of it.

The figure measured is the sum of uncompressed payload sizes, which is a proxy for the work of loading rather than for disk or memory. Compression usually puts the file itself at a fraction of that number.

Saving carries no such cap, so a long session can grow past one that is set too low. The storage writer warns once when a session reaches 80 percent of the ceiling, while there is still room to act. Trim that session, fork it to carry the useful part forward, or raise the ceiling. `CAUDRA_MAX_EAGER_LOAD_MB` raises it for a single run, which is the quickest way to reach a session that already refuses to open.

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

Both policies start empty, so Caudra keeps every session until you ask it to do otherwise:

```lua
caudra.setup({
    storage = {
        retention = {
            group_by = "directory",
            sweep_interval_hours = 24,
            trim = {},
            forget = {},
        },
    },
})
```

To keep the twenty most recently active sessions of every directory in full and strip the artifacts of anything older than ninety days, set `trim = { keep_last = 20, keep_within = "90d" }`. To delete sessions after two years, set `forget = { keep_within = "2y" }`. Set `sweep_interval_hours = 0` to run retention only through the CLI.

The sweep runs on a background thread once per interval while the TUI is open. It trims, forgets, and then prunes: due cleanup jobs run, orphaned artifact directories older than seven days are removed, the write-ahead log is checkpointed, and free pages are returned to the filesystem. With the policies left empty the sweep only prunes, which reclaims space and leaves session data alone. Every step is transactional or idempotent, so an interrupted sweep leaves nothing inconsistent.

A schema migration prunes as its last step. Rewriting rows puts the pages they occupied on the free list, so an upgrade that skipped this could leave the file larger than the data it holds until the next sweep came due.

`caudra storage trim --dry-run` and `caudra storage forget --dry-run` print the plan with the reason each session is kept. `caudra storage sessions` lists sessions with their tier. `caudra storage pin <ID>` keeps a session regardless of policy. See [CLI](/docs/cli/#caudra-storage) for every flag.

Retention never removes spending records. What a session cost is written to a separate ledger that no session owns, so `caudra storage usage` still answers after the sessions are gone. See [Lifetime spend](/docs/token-economy/#lifetime-spend).

## What the state directory holds

`/storage` shows where the disk went. A proportional bar splits the state directory between the session database, tool output files, workspace snapshots, and archives, and a legend gives each one its exact size. Below it, the database section reports file and write-ahead log sizes, free pages that a prune would reclaim, and the session and item counts behind them.

The snapshot section lists workspace stores largest first with their size, object count, and number of snapshots. A store whose workspace root is gone is marked rather than hidden, because an orphaned store is usually the one worth deleting. `/storage all` lists every store instead of the largest few; the footer command toggles between them.

Measuring walks the snapshot stores on disk, so the modal opens immediately and fills in when the walk finishes. The same figures are available without the TUI from `caudra storage stats` and `caudra storage snapshots`.

To reclaim one store the modal named, trim that session: `caudra storage trim <ID>`. Trimming drops the workspace snapshots, retained tool output files, rewind archives, and the journals and timelines of its workflow runs while the conversation stays and stays resumable, so it is the right answer when a single session has grown out of proportion and you still want its transcript. Pinned sessions are refused and a session open in another process is skipped, so the command is safe to run while Caudra is up. Add `--dry-run` to see the session and its artifact size first.

## Conversation revert

A conversation revert changes the active head rather than deleting items. Selecting a user prompt lands before that prompt and restores it to the composer. Other item types are inclusive.

Sending a prompt after revert creates a new branch from that point. The abandoned branch remains stored. Unrevert is available until new work commits the new branch.

`Esc Esc` while idle uses the same conversation-only mechanism through the rewind picker.

## File snapshots

Caudra captures the session-start snapshot on the first tool call that may change a file, together with the history head that call's run started from. A turn that only talks, and a turn whose tools only read, capture nothing and leave no store on disk. After each run completes or is cancelled, Caudra snapshots the resulting head, and it does the same on exit, in both cases only for a session that already has a session-start snapshot to bracket.

A capture must finish before the call that triggered it runs, so the file it is about to overwrite is recorded first. If the capture fails, that one tool call fails and the rest of the turn continues. Parallel calls share one capture: the first to arrive takes it and the others wait.

A file restore selects the nearest available snapshot at or before the chosen item, so several parallel tool calls share one safe run checkpoint.

Snapshots are content-addressed with SHA-256 and stored under the Caudra state directory in `session-snapshots/<session-id>/<workspace-hash>/`. The object store contains the complete file bytes under their hashes. A checkpoint manifest maps each relative path to its object hash and Unix mode. Unchanged files reuse the same object instead of storing another copy.

### Limits

Snapshot walks follow `.gitignore`, `.ignore`, the global Git ignore file, and `.git/info/exclude`, whether or not the workspace is a Git repository. Nested repositories, `.git`, symlinks, special files, and paths outside the session directory are not captured, and the walk does not cross a filesystem boundary. Changing a path between captured and ignored or symlink state is outside the restore guarantee because manifests cannot distinguish that state from absence.

Objects are stored uncompressed, so a workspace larger than its retention target would sit over budget from the first capture. Caudra measures the tree while walking it and refuses one that does not fit:

| Setting | Default | Effect |
| --- | --- | --- |
| `storage.snapshots.enabled` | `true` | `false` turns capture off for every workspace |
| `storage.snapshots.max_bytes_mb` | `512` | Both the walk ceiling and the retention target |
| `storage.snapshots.max_files` | `50000` | Walk ceiling on file count |
| `storage.snapshots.max_file_bytes_mb` | `100` | Files above this are skipped, and the rest of the tree is still captured |

Caudra also refuses a filesystem root and a home directory outright.

A refusal costs file revert rather than the user's work: the tool call proceeds, Caudra reports the reason once, and `/storage` shows it alongside the empty store. The verdict is decided once per workspace and is not re-paid on later calls. Conversation revert is unaffected.

Old checkpoint manifests are removed first when a store passes its retention target. The session-start anchor and data needed by an active revert remain available, so protected data can exceed the target.

Restore compares the current file hash and mode with the source snapshot. If a tracked path changed outside the captured run, restore aborts and reports a conflict. Caudra does not overwrite it automatically. Conversation-only revert remains available when file restore cannot proceed.

Only paths that differ between the source and target manifests are touched. Files created after the target are deleted, deleted files are recreated, and unrelated files remain in place. Each file replacement is atomic. A restore spanning several files completes through the journal.

Before applying changes, Caudra captures the current state of every affected path. Unrevert restores this state before moving the conversation head back. A restore journal records prepare, apply, and verification phases so a later restore can finish an interrupted transaction.

Snapshots cover regular files. They cannot reverse running processes, databases, network calls, Git branches or index state, nested repository state, or commands that changed files outside the session directory. Changes from manual shell commands can appear as conflicts. `/cd` changes the process workspace for every live session and is blocked while any session is busy or has a pending revert.
