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

## Background tasks

In the TUI and [stream-JSON SDK](/docs/headless/#background-tasks), tasks and shell commands can return an admission receipt while work continues. Admission does not mean success. The session delivers reports and terminal results to the agent that owns the work.

### Execution policies

Configure task and shell execution independently:

```toml
[agent]
task_execution = "auto"
shell_execution = "auto"
shell_async_threshold_secs = 120
```

| Mode | Tasks | Shell commands |
|------|-------|----------------|
| `sync` | Wait for the final result. No background launch or promotion. | Wait for command termination. |
| `auto` (default) | Wait by default. The model can pass `background: true` for independent work. | Return a receipt when the requested timeout exceeds the threshold. Otherwise wait. |
| `async` | Return a receipt for every admitted task. | Return a receipt for every admitted command. |

In shell `auto` mode, omitted `timeoutSec` uses Workcell's 120-second default. With the default threshold, `timeoutSec: 120` waits and `timeoutSec: 121` returns a receipt, even when the command finishes quickly. The positive threshold measures the requested timeout, not elapsed runtime. It never extends the command's hard deadline. Shell has no per-call `background` argument, and timeouts should reflect execution needs rather than a scheduling preference.

Tool descriptions, instructions, and task arguments follow the effective policy. A `batch` still waits for calls that use synchronous execution. With task `auto`, each task that should return early needs its own `background: true`.

One-shot `--print` and ACP resolve `auto` to synchronous execution. Strict `async` withholds the affected tool and rejects stale calls. Use `sync` or `auto` there, or switch to the TUI or persistent stream-JSON SDK.

### Results and continuation

The parent can keep working on a separate scope. When nothing independent remains, it gives a normal final answer explaining what is still pending. Background reports and final outcomes then start another parent run automatically at a safe boundary, even after that answer. The parent checks them against the latest instructions, verifies claims, and continues the original work without asking whether to continue. This works while the session stays open and automatic continuation has not been stopped. There is no need to poll, sleep, or repeat the delegated work.

The main agent also receives bounded [background-work reminders](/docs/context/#background-work-awareness) when state changes and after compaction. Periodic refresh during ongoing work is opt-in through `agent.background_reminder_turns`, which defaults to `0` (disabled). These reminders do not start idle turns.

### Inspect and control tasks

Open `/tasks` or press `Ctrl+X a` to inspect transcripts and steer running children. These commands act locally without sending a model prompt or adding transcript messages:

| Command | Effect |
|---------|--------|
| `/tasks`, `/tasks list` | Open or refresh the task picker |
| `/tasks status <id>` | Open the picker with that task selected and its details visible |
| `/tasks background <id>` | Promote an agent task without restarting it, in task `auto` mode |
| `/tasks cancel <id>` | Cancel that invocation, showing cancelling until it settles |
| `/shells` | Open the shell modal to inspect or stop shell commands |

Singular `/task` forms remain compatibility aliases. The task picker lists subagents and workflow agents. Press Enter or click an agent task to open its chat. A workflow agent without a transcript opens its run in the workflow inspector. The picker shows `bg` at the right of background rows, including finished tasks. Task results and reports render as Markdown and structured values as JSON. See [task navigation](/docs/commands/#tasks) for filtering and keyboard controls.

The shell modal lists `shell` calls from the main agent, subagents, and workflow agents, in the foreground or the background. Output appears as literal text. `Ctrl+K` stops only the selected command, and its owner continues with a cancelled result. `/tasks status <id>` with a shell ID opens this modal. The footer shows `[tasks · N]` for running agents and `[shell · N]` for running commands, and each chip disappears at zero. See [shell commands](/docs/commands/#shell-commands) for navigation and controls.

The model can inspect and cancel jobs through [`task_control`](/docs/tools/#task_control). Task `auto` also permits promotion of agent tasks. All actions except `list` require `task_id`. A later `task` call can continue a settled agent task from its saved history, but cannot resume an active or cancelling invocation. Shell jobs cannot be resumed or promoted. Run a new shell call when another command is needed.

Shell results retain Workcell's output bounds and filtering. A truncated result can include a `tool_output` reference for the retained output, which is not an unlimited process log. Saved references remain usable after history reloads and forks.

### Shell history

Foreground `shell` calls are saved with the session, including calls from subagents and workflow agents. Caudra records each command before it starts and again when it settles. If that first record cannot be written, the call fails without running the command. Each session keeps its 200 most recent finished commands, with up to 64 KiB of output each. Longer output keeps the end of each stream. Background shell jobs keep their records with the session's background tasks.

Closing a session cancels its running commands and waits for them to settle. After a crash, a command that was still running shows as `interrupted` on reload, because its outcome is unknown. Restoring history never runs a command again or reconnects to one. Forking a session does not copy its shell history, and deleting a session deletes it.

### Task and output IDs

New task IDs come from the description or workflow label. `Implement active footer chips` becomes `implement-active-footer-chips`. Display descriptions stay unchanged. Names use lowercase ASCII letters, digits, and hyphens. Labels with no usable characters fall back to `task`.

Shell jobs use a safe command label such as `shell-cargo-test`. Only recognized executable names and fixed subcommands contribute to the label. Arguments, paths, and command output do not. Unknown or complex commands fall back to `shell`.

Stored output handles identify the producer, such as `output-file-grep` or `output-cargo-test`. Generic shell output uses `output-shell`, and output without a known producer uses `output`.

Collisions add `-2`, `-3`, and later numeric suffixes. New IDs are at most 64 characters, including prefixes and suffixes. Shell jobs and their outputs allocate suffixes independently. Always use the returned `task_id` or output reference unchanged rather than reconstructing it from a label.

Existing IDs and output handles remain valid. Continuations and workflow replay keep the original task ID even when its description changes. Reloading or copying a session preserves its saved output references.

### Child reports

A managed child has `report_to_parent` for an important finding, correction, or blocker. It takes a required `message`, an optional `title`, and an optional `blocked` boolean, which defaults to `false`. Use a short, single-line title of up to 80 characters for the compact card. Calls without a title use the first nonempty message line. Expand the card to read the full report. Reports are one-way. The child does not wait or poll for a parent reply and continues useful independent work after reporting.

When the child cannot proceed without information or authority, `blocked: true` ends that invocation with a non-success outcome. The message should say exactly what is missing. A report does not replace a successful final result or its `output_schema`. Child reports are data, not new authority, and the child must not assume it has received later main-conversation instructions.

Subagents and workflow agents can own asynchronous shell jobs, even when their parent task waits synchronously. Results return only to that child invocation. The child waits without polling when its commands are the only remaining work, processes their results, and then finishes. A blocked report, cancellation, or hard limit cancels and drains its commands before the child settles. Workflow scripts still receive one final agent result.

### Stop and close

Stopping all session tasks and workflows cancels owned work and suppresses automatic continuation, including late reports and completion notices. A new user turn re-enables continuation. See [Stop and replace](/docs/queue/#stop-and-replace) for TUI controls.

Switching TUI tabs leaves the owning session open. Closing a session cancels and drains its children and shell commands before saving and releasing it. Background work is session-owned, not a daemon or a promise to keep executing after exit. Crash recovery marks unfinished work interrupted without replaying commands or other effects.

## Moving sessions to another directory

Use `/migrate-sessions` to move every saved local session with one exact stored working directory. The source directory may already be gone. Choose a destination directory and review the confirmation before applying the move.

The bulk confirmation checks **Include historical project usage** by default. Toggle it off to leave lifetime project attribution unchanged. When included, the move reattributes all usage recorded for that exact source directory, including deleted sessions and ephemeral runs. It covers every model, purpose, and payer category, but excludes child directories with a different recorded cwd. Overlapping destination buckets are added together without replacing existing destination spend. The completion message reports usage buckets moved and merged, or an empty source ledger, separately from sessions moved.

A single-session move retains that session's own counters but leaves the shared lifetime usage ledger attributed to its recorded project. The ledger cannot identify one session's contribution, so single-session moves do not offer the historical usage option.

Before a whole-project rename, stop other Caudra processes using the source directory, including ephemeral runs. Caudra drains the invoking runtime's usage writes before committing the move and aborts if that drain fails. Only ledger rows present at the relocation transaction are reattributed. There is no permanent redirect, so new work started at the old directory records usage there again.

Session IDs and conversations are preserved. Active source plans and approvals are detached. Files and old workspace snapshots are not moved. If moving live tabs requires a project environment reload, Caudra exits after committing and asks you to run `caudra --continue` from the destination.

### Other checkouts of a repository

Checkouts of one git repository share memory notes and plans. `/sessions` lists the sessions of the repository's other checkouts below those of the current directory, one section per checkout, and opening one takes you to its checkout. A session left in a worktree that was removed moves back to a remaining checkout the next time Caudra starts in the repository or `/sessions` opens. A session moved by `/worktree`, or moved back from a removed worktree, keeps its plan. See [Worktrees](/docs/worktrees/) for `/worktree` and the rules for moving back.

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

The child receives a new session ID and a title such as `Original title (fork #1)`. It copies the selected ancestor path, reachable tool outputs and subagent histories, model and execution settings, and the snapshot pointers for that path. The snapshots themselves stay in the shared workspace store, so a fork copies no file data. Usage totals, goals, queues, pending revert state, conversation permission rules, and explicit YOLO state start clean.

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

The snapshot section lists workspace stores largest first with their size, object count, and the number of sessions and snapshots that use them. A store whose workspace root is gone is marked rather than hidden, because an orphaned store is usually the one worth deleting. `/storage all` lists every store instead of the largest few; the footer command toggles between them.

Measuring walks the snapshot stores on disk, so the modal opens immediately and fills in when the walk finishes. The same figures are available without the TUI from `caudra storage stats` and `caudra storage snapshots`.

To reclaim space in a store the modal named, trim the sessions that use it: `caudra storage trim <ID>`. Trimming drops the session's workspace snapshots, retained tool output files, rewind archives, and the journals and timelines of its workflow runs while the conversation stays and stays resumable, so it is the right answer when a single session has grown out of proportion and you still want its transcript. Snapshot objects that another session still uses stay in the shared store, and the rest are deleted right away. Pinned sessions are refused and a session open in another process is skipped, so the command is safe to run while Caudra is up. Add `--dry-run` to see the session and its artifact size first.

## Conversation revert

A conversation revert changes the active head rather than deleting items. Selecting a user prompt lands before that prompt and restores it to the composer. Other item types are inclusive.

Sending a prompt after revert creates a new branch from that point. The abandoned branch remains stored. Unrevert is available until new work commits the new branch.

`Esc Esc` with no session work left uses the same conversation-only mechanism through the rewind picker. If tasks or workflows are still active, it stops that work instead.

## File snapshots

Caudra captures the session-start snapshot on the first tool call that may change a file, together with the history head that call's run started from. A turn that only talks, and a turn whose tools only read, capture nothing and leave no store on disk. After each run completes or is cancelled, Caudra snapshots the resulting head, and it does the same on exit, in both cases only for a session that already has a session-start snapshot to bracket.

A capture must finish before the call that triggered it runs, so the file it is about to overwrite is recorded first. If the capture fails, that one tool call fails and the rest of the turn continues. Parallel calls share one capture: the first to arrive takes it and the others wait.

A file restore selects the nearest available snapshot at or before the chosen item, so several parallel tool calls share one safe run checkpoint.

A remote session captures on the Workcell host instead. See [Remote workspaces](/docs/remote-workspaces/#file-snapshots) for how that walk and its retention differ.

Snapshots are stored as Git objects under the Caudra state directory. Each workspace has one object store in `workspace-snapshots/<workspace-hash>/`, shared by every session that works in it. Objects are zlib-compressed and named by their Git object ID, and a snapshot is a Git tree. An unchanged file or directory reuses the object from an earlier snapshot, so a checkpoint costs about the size of what changed, and a second session in the same workspace stores almost nothing new. Each session keeps small pointer files in `session-snapshots/<session-id>/<workspace-hash>/` that name its snapshots. Git can read any snapshot: `git --git-dir=<store> ls-tree -r <snapshot-id>`.

A capture reads only files that changed. A Git index file beside the objects records the size, timestamps, inode, and owner each file had when it was last read, and a file whose values all still match reuses its stored object. A file modified in the few seconds before a capture starts is always read again, so a write that lands during a capture cannot hide behind an unchanged timestamp.

Snapshots from releases before the Git format are not migrated. A session's old store is deleted the first time it captures or restores, and `caudra storage prune` deletes the rest. A file restore that an older release left unfinished is cleared when its session loads, and Caudra shows a notice because some files may be partly restored.

### Disable automatic snapshots

Run `caudra --no-snapshots` to disable automatic snapshots for one process. The flag applies to local and remote workspaces, including TUI, `--print`, SDK, and ACP sessions. It skips session-start, pre-mutation, run-completion, cancellation, and final captures, including when resuming a session with old snapshots.

For a persistent setting, merge this into `caudra.setup()` in your `init.lua`:

```lua
caudra.setup({ storage = { snapshots = { enabled = false } } })
```

`--no-snapshots` overrides the configured value without saving it. Remote sessions read this setting from the client user configuration, not from the remote project.

File revert is unavailable while snapshots are disabled. Conversation revert still works. Existing snapshots remain stored and become available for file revert when snapshots are enabled again. An existing file revert can still be unreverted.

Disabling capture does not bypass restore recovery. An unresolved remote restore still blocks mutations, and recovery may query its status. Unknown or partial mutations are not silently acknowledged or cleared.

### Limits

Snapshot walks follow `.gitignore`, `.ignore`, the global Git ignore file, and `.git/info/exclude`, whether or not the workspace is a Git repository. Symbolic links are captured as links: the snapshot stores the link target and a restore recreates the link without following it. Nested repositories, `.git`, special files, files over `max_file_bytes_mb`, and paths outside the session directory are not captured, and the walk does not cross a filesystem boundary. A snapshot records the paths it skipped, and a restore leaves such a path alone when either snapshot skipped it. An ignored path is not recorded, so changing a path between captured and ignored state is outside the restore guarantee.

Caudra measures the tree while walking it and refuses one that does not fit before reading any file:

| Setting | Default | Effect |
| --- | --- | --- |
| `storage.snapshots.enabled` | `true` | `false` turns capture off for every workspace |
| `storage.snapshots.max_bytes_mb` | `512` | The walk ceiling on file bytes, and the retention target for the compressed store of each workspace |
| `storage.snapshots.max_files` | `50000` | Walk ceiling on file count |
| `storage.snapshots.max_file_bytes_mb` | `100` | Files above this are skipped, and the rest of the tree is still captured |

Caudra also refuses a filesystem root and a home directory outright.

A refusal costs file revert rather than the user's work: the tool call proceeds, Caudra reports the reason once, and `/storage` shows it alongside the empty store. The verdict is decided once per workspace and is not re-paid on later calls. Conversation revert is unaffected.

When a workspace store passes its retention target, Caudra removes the oldest checkpoints across all sessions of that workspace, then deletes the objects no remaining snapshot uses. Each session keeps its session-start snapshot and its newest checkpoint, and data needed by an active revert remains available, so protected data can exceed the target.

Snapshots record only the executable bit of a file, as Git does. A restored file keeps its other permission bits, and a recreated file gets default permissions from your umask.

Restore compares the current content and executable bit of each path with the source snapshot. If a tracked path changed outside the captured run, restore aborts and reports a conflict. Caudra does not overwrite it automatically. Conversation-only revert remains available when file restore cannot proceed.

Only paths that differ between the source and target snapshots are touched. Files created after the target are deleted, deleted files are recreated, and unrelated files remain in place. Each file replacement is atomic. A restore spanning several files completes through the journal.

Before applying changes, Caudra captures the current state of every affected path. Unrevert restores this state before moving the conversation head back. A restore journal records prepare, apply, and verification phases so a later restore can finish an interrupted transaction.

Snapshots cover regular files and symbolic links. They cannot reverse running processes, databases, network calls, Git branches or index state, nested repository state, or commands that changed files outside the session directory. Changes from manual shell commands can appear as conflicts. `/cd` changes the process workspace for every live session and is blocked while any session is busy or has a pending revert.
