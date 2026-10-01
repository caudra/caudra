+++
title = "Sessions, Forks, and Revert"
weight = 35
[extra]
group = "Guides"
+++

# Sessions, forks, and revert

Caudra stores conversation history as parent-linked items. User prompts, assistant text, reasoning, tool calls, and tool results are separate items. A session head selects the active path through those items. Moving the head keeps the abandoned path available for unrevert and later forks.

For remote sessions, transcripts stay on the client and [file change records](#file-revert) stay on the Workcell host. Resume requires the original remote workspace identity and generation. See [Remote session identity and recovery](/docs/remote-workspaces/#cwd-and-resume) before moving an endpoint, replacing a workspace, or recovering an interrupted mutation. The `storage` subcommand is disabled when a remote Workcell selector is supplied.

Managed sandbox sessions retain their sandbox source. `caudra --session ID` resolves it before validating the remote workspace, and a paused VM requires explicit resume approval. See [sandbox conversation and VM resume](/docs/sandboxes/#resume-a-conversation-or-vm). Saving or deleting conversation history does not delete the VM or extend its disk-retention deadline.

Resuming a session bound to a managed sandbox needs `experimental.sandboxes`, and one bound to a direct remote workspace needs `experimental.remote_workcell`. The rule covers `--continue`, `--session`, the session picker, the SDK, and ACP. Without the switch, resume fails with an error and never falls back to local execution. The session data stays intact, so the session resumes once the switch is on. See [Experimental features](/docs/configuration/#experimental-features).

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

## Cross-session messaging

The experimental messaging MVP lets live main sessions on the same Unix host exchange text, including sessions in separate terminals or TUI tabs. It supports the TUI and an active one-shot `--print` run. Subagents, the SDK, ACP, remote Workcell sessions, and managed sandbox sessions are outside this scope.

Messaging is off by default. Enable it in the **global** `caudra.toml` and restart each participating Caudra process:

```toml
[experimental]
cross_session_messaging = true

[agent.messaging]
inbound = "auto"
```

A project cannot enable the experiment. An inbound setting or saved session cannot enable it either. With the switch off, Caudra creates no messaging endpoint and exposes no messaging tools or peer-triggered wakes. Previously recorded messages remain readable.

### Find peers and review messages

`/peers` opens the Sessions view of the peer manager. `/messages` opens its Held messages view. Switch between them with `1` and `2`. See [inbound policy and trust](/docs/permissions/#cross-session-messages) before allowing automatic delivery.

Sessions shows a discovery snapshot of eligible live peers. Select a row to inspect its workspace, activity, inbound policy, and exact target. `Ctrl+R` refreshes without blocking the interface. `Ctrl+B` copies the target. A failed refresh keeps the previous snapshot visible with an error.

Press `/` to filter the current list, then Enter to leave filter editing. Enter on a held message opens its review. Read the literal message body, then use `y` to approve once or `n` to review rejection. Rejecting removes the message from the live inbox. Browsing, filtering, and refreshing grant no approval. Tab switches list/detail focus. Esc backs out before closing. Narrow terminals show one pane at a time.

The Held messages view contains messages waiting for this session's review or delivery limits. Recorded messages and send receipts remain in the transcript. Use the agent to send messages.

| Command | Action |
|---|---|
| `/peers` | Open the Sessions view |
| `/messages` | Open the Held messages view |
| `/messages approve <id>` | Approve a message from the current review |
| `/messages reject <id>` | Reject a message from the current review |
| `/messages inbound auto\|accept\|hold\|refuse` | Set the session policy within project restrictions |

Open the individual message's review before using an approve or reject command. Review again if the session's mode, workspace, or policy changes. An old review cannot approve a message under new controls.

Press `p` outside filter editing to manage this session's inbound policy. Select an option and press `a` to apply it. Relaxing the policy requires confirmation because it can release held messages and start billable turns. Project restrictions remain in force. This control never changes the selected peer's policy.

You can also ask the agent to find a session and send it a message. It uses `list_sessions` for discovery and `send_message` for delivery. Discovery cards show session labels, word-based targets, workspaces, and availability, without transcript previews. A title is not a unique address.

Use the exact target from discovery or an incoming reply address. Targets belong to your current live registration and are never reassigned to a replacement peer. Discover again after restarting or replacing your session. Message names also use generated words, including the names shown by `/messages` for approval or rejection.

Accepted messages enter at a safe run boundary. They can also wake an eligible idle TUI session and start a billable model turn. They do not interrupt a running tool or bypass cancellation, permission review, or delivery limits. The recipient still applies its own tool permissions.

Approval can leave a message held when its delivery budget is exhausted. Opening or closing the manager does not reset that budget or resume cancelled work. An idle session waits until the modal closes before starting a peer-triggered turn. Work already running keeps its existing safe-boundary delivery behavior.

### Delivery receipts and lifetime

| Status | Meaning |
|---|---|
| `queued` | Accepted into the live inbox, not yet delivered to the model |
| `held` | Accepted into the live inbox, waiting for approval or an automatic-delivery limit to clear |
| `refused`, `unavailable`, `rate_limited` | Not admitted |
| `unknown` | Delivery may have been accepted before the connection failed |

A receipt does not promise a reply or completed work. Do not treat `unknown` as a definite failure and send the same request again under a new identity.

Queued and held messages live only in bounded memory. Closing or replacing the receiving session, exiting, or crashing can discard them. There is no offline inbox or crash-durable delivery guarantee. Messages already recorded in conversation history follow normal session retention. Reloading or rewinding history never sends them again.

Each body is limited to 32 KiB of UTF-8. The inbox admits at most 50 messages across pending, held, and claimed states, with a 1 MiB session ceiling and an 8 MiB process ceiling. A full inbox rejects new messages rather than evicting older ones.

Each session can automatically deliver 16 messages and send 16 messages between local user interactions. This shared budget covers all peers and both busy delivery and idle wakes. Exhaustion holds further incoming messages and rejects further sends. Local user input resets the budget. Peer replies, elapsed time, and reloading the session do not.

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

The footer keeps a differently styled spinner and a waiting label while background work or result delivery will wake the main agent. It does not mean a model request is running. Queued next messages and goal checks wait until that work settles and the main agent processes its results. See [Queue and Steering](/docs/queue/) for guide and replace behavior.

### Inspect and control tasks

Open `/tasks` or press `Ctrl+X a` to inspect transcripts and steer running children. These commands act locally without sending a model prompt or adding transcript messages:

| Command | Effect |
|---------|--------|
| `/tasks`, `/tasks list` | Open or refresh the task picker |
| `/tasks status <id>` | Open the picker with that task selected and its details visible |
| `/tasks background <id>` | Promote an agent task without restarting it, in task `auto` mode |
| `/tasks cancel <id>` | Cancel that invocation, showing cancelling until it settles |
| `/shells` | Open the shell modal to inspect or stop shell commands |

The task picker lists subagents and workflow agents. Press Enter or click an agent task to open its chat. A workflow agent without a transcript opens its run in the workflow inspector. The picker shows `bg` at the right of background rows, including finished tasks. Task results and reports render as Markdown and structured values as JSON. See [task navigation](/docs/commands/#tasks) for filtering and keyboard controls.

The shell modal lists `shell` calls from the main agent, subagents, and workflow agents, in the foreground or the background. Output appears as literal text. `Ctrl+K` stops only the selected command, and its owner continues with a cancelled result. `/tasks status <id>` with a shell ID opens this modal. The footer shows `[tasks · N]` for running agents and `[shell · N]` for running commands, and each chip disappears at zero. See [shell commands](/docs/commands/#shell-commands) for navigation and controls.

The model can inspect and cancel jobs through [`task_control`](/docs/tools/#task_control). Task `auto` also permits promotion of agent tasks. All actions except `list` require `task_id`. A later `task` call can continue a settled agent task from its saved history, but cannot resume an active or cancelling invocation. Shell jobs cannot be resumed or promoted. Run a new shell call when another command is needed.

Settled tasks and background shell jobs are archived with the session once delivery is complete. Finished history does not consume live admission capacity. In either modal, `Alt+Right` loads older archived entries and `Alt+Left` returns to recent history. Details and task transcripts load on demand. Old IDs, results, and continuation history remain available until the session is deleted or removed by retention. The model can page history with the `next` cursor returned by `task_control list`, passing it as `before` on the next call.

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

Workflow run IDs use random word-list names like plan files, such as `neat-wanted-cowbird`. Collisions add numeric suffixes within the same 64-character limit. Existing workflow IDs remain valid, and pause, resume, and history keep the original ID.

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

Session IDs and conversations are preserved. Active source plans and approvals are detached. Files are not moved, and file changes made before the move can no longer be reverted. If moving live tabs requires a project environment reload, Caudra exits after committing and asks you to run `caudra --continue` from the destination.

### Other checkouts of a repository

Checkouts of one git repository share memory notes and plans. `/sessions` lists the sessions of the repository's other checkouts below those of the current directory, one section per checkout, and opening one takes you to its checkout. A session left in a worktree that was removed moves back to a remaining checkout the next time Caudra starts in the repository or `/sessions` opens. A session moved by `/worktree`, or moved back from a removed worktree, keeps its plan. See [Worktrees](/docs/worktrees/) for `/worktree` and the rules for moving back.

## Ephemeral sessions

Run `caudra --ephemeral` for a session that leaves no session record behind. Set `storage.ephemeral = true` to make this the default.

Caudra creates a private temporary state root under `XDG_RUNTIME_DIR` or the [scratch directory](/docs/configuration/#directory-layout). Session rows, tool outputs, file change records, input history, and stashed prompts use that root. It is removed when Caudra exits through its normal success or error paths. A forced process kill can leave the temporary root for the operating system to clean up.

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
- **Revert both** moves the conversation head and reverts the file changes recorded after the message.
- **Revert conversation** moves only the conversation head.
- **Revert files** reverts only the recorded file changes.
- **Unrevert** puts back the head and the files from before the revert.

**Revert both** and **Revert files** appear only when a [file revert](#file-revert) can run from that message. Otherwise the menu title says why. Revert and unrevert require every live session in the workspace to be idle. Active shell commands and subagents also block them. Forking never starts a model request.

## Fork boundaries

| Selected item | New session history | Composer |
|---|---|---|
| User prompt | Stops before the prompt | Prompt text and images restored for editing |
| Assistant text | Includes the selected text item | Empty |
| Reasoning | Includes the selected reasoning item | Empty |
| Completed tool | Includes the call and result | Empty |
| Incomplete tool call | Includes the call; history repair supplies an unavailable result | Empty |

The child receives a new session ID and a title such as `Original title (fork #1)`. It copies the selected ancestor path, reachable tool outputs and subagent histories, and model and execution settings. It also holds the parent's [file change records](#file-revert), so the child can revert files too, and it copies no file data. Usage totals, goals, queues, pending revert state, conversation permission rules, and explicit YOLO state start clean.

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

Sessions have two tiers. A **full** session keeps everything: the conversation, rich tool output records, retained tool output files, rewind archives, and its file change records. A **transcript** session keeps the conversation, subagent transcripts, usage, model, mode, drafts, queue, and permission rules, and can still be resumed. It cannot revert files to its earlier messages, has no `tool_output_read` access to old outputs, and renders old tool calls from their model-facing text. Small structured records such as todo lists stay.

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

```toml
[storage.retention]
group_by = "directory"
sweep_interval_hours = 24
trim = {}
forget = {}
```

To keep the twenty most recently active sessions of every directory in full and strip the artifacts of anything older than ninety days, set `trim = { keep_last = 20, keep_within = "90d" }`. To delete sessions after two years, set `forget = { keep_within = "2y" }`. Set `sweep_interval_hours = 0` to run retention only through the CLI.

The sweep runs on a background thread once per interval while the TUI is open. It trims, forgets, and then prunes: due cleanup jobs run, orphaned artifact directories older than seven days are removed, the file change records of deleted sessions older than seven days are released, each change store is trimmed to its size budget, the write-ahead log is checkpointed, and free pages are returned to the filesystem. With the policies left empty the sweep only prunes, which reclaims space and leaves session data alone. Every step is transactional or idempotent, so an interrupted sweep leaves nothing inconsistent.

A schema migration prunes as its last step. Rewriting rows puts the pages they occupied on the free list, so an upgrade that skipped this could leave the file larger than the data it holds until the next sweep came due.

`caudra storage trim --dry-run` and `caudra storage forget --dry-run` print the plan with the reason each session is kept. `caudra storage sessions` lists sessions with their tier. `caudra storage pin <ID>` keeps a session regardless of policy. See [CLI](/docs/cli/#caudra-storage) for every flag.

Retention never removes spending records. What a session cost is written to a separate ledger that no session owns, so `caudra storage usage` still answers after the sessions are gone. See [Lifetime spend](/docs/token-economy/#lifetime-spend).

## What the state directory holds

`/storage` shows where the disk went. A proportional bar splits the state directory between the session database, tool output files, file change records, and archives, and a legend gives each one its exact size. Below it, the database section reports file and write-ahead log sizes, free pages that a prune would reclaim, and the session and item counts behind them.

The change record section lists the stores largest first, one per workspace directory, with their size, object count, records, holding sessions, open records, and pending reverts. A store whose directory is gone, or that no session works in any more, shows its key instead. A store that no existing session holds is marked as orphaned rather than hidden. `/storage all` lists every store instead of the largest few, and the footer command toggles between them. When file revert is unavailable for the current session, the modal says why.

Measuring walks the stores on disk, so the modal opens immediately and fills in when the walk finishes. The same figures are available without the TUI from `caudra storage stats` and `caudra storage snapshots`.

Recording keeps each store near its [size budget](#limits) on its own. To reclaim more, trim the sessions that use a store: `caudra storage trim <ID>`. Trimming releases the session's local file change records and drops its retained tool output files, rewind archives, and the journals and timelines of its workflow runs. The conversation stays and stays resumable. Records that another session still holds, such as a fork, stay in the store, and the rest are deleted. Pinned sessions are refused and a session open in another process is skipped, so the command is safe to run while Caudra is up. Add `--dry-run` to see the session and its artifact size first.

## Conversation revert

A conversation revert changes the active head rather than deleting items. Selecting a user prompt lands before that prompt and restores it to the composer. Other item types are inclusive.

Sending a prompt after revert creates a new branch from that point. The abandoned branch remains stored. Unrevert is available until new work commits the new branch.

`Esc Esc` with no session work left uses the same conversation-only mechanism through the rewind picker. If tasks or workflows are still active, it stops that work instead.

## File revert

File revert undoes the file changes that this session's tool calls made after the chosen message, newest first. Files that no record names stay as they are, including your own edits. If the agent edits `a.rs` and `b.rs` and you then edit `c.rs`, reverting to the prompt that started the agent's turn restores `a.rs` and `b.rs` and leaves `c.rs` alone.

The chosen message sets the boundary as in a [conversation revert](#conversation-revert). A user prompt reverts the changes of its own turn too, and any other item keeps the changes made up to it. A message that compaction rewrote counts from its original. Caudra refuses a file revert at a rewritten message whose original it cannot find.

The first **Revert files** or **Revert both** only previews. It counts the files the revert would create, replace, and delete, and the later calls that ran without a record and keep their changes. Repeat the same action to run it. Any other revert or an unrevert drops the preview.

Before writing anything, the revert checks every path it would touch. A path is a conflict when the file changed since it was recorded (`changed`), when another change landed between two of the records (`interleaved`), or when a record could not store it (`unrecorded`). One conflict aborts the whole revert, conversation half included, and the notice lists the conflicts by kind and path. **Revert conversation** still works.

Caudra also refuses a file revert, and names the reason, when the records it needs were dropped to keep the store within its [size budget](#limits), when calls of this session are still changing files, or when an earlier file revert did not finish.

A session's records reach back only to the point where its current store took over. That point moves forward when recording comes back on after being off, including after a local `--print`, SDK, or ACP run, and when the session moves to another directory or remote workspace. A session saved by an older version starts at its first load. A fork starts where its parent does. A file revert to a message before that point is refused.

**Unrevert** puts the reverted files back, then moves the conversation head back. Reverts made one after another stack, and Unrevert undoes all of them. A file that changed since the revert is a conflict, and then nothing is unreverted. New work in the session keeps the reverted files as they are and ends unrevert.

If Caudra stops during a file revert, the store works out from the records and the files how far it got the next time it opens. The session keeps that revert pending: Unrevert puts back what it changed, and new work keeps the files as they are. A revert that finished after Caudra stopped is adopted with a notice, and the conversation stays where it was. A call that was still running when Caudra stopped counts as a call without a record.

Records are not migrated from the workspace snapshots of earlier versions. A file revert that an older version left pending is cleared when its session loads, with a notice, and can no longer be unreverted. If Caudra stopped during that revert, some files may be partly restored. `caudra storage prune` and the TUI sweep delete the old `session-snapshots/` and `workspace-snapshots/` directories.

File revert covers regular files and symbolic links in the session directory. It cannot reverse running processes, databases, network calls, Git branches or index state, nested repository state, or changes outside the session directory. `/cd` changes the process workspace for every live session and is blocked while any session is busy or has a pending revert.

A remote session records on the Workcell host. See [Remote workspaces](/docs/remote-workspaces/#file-revert) for what differs.

### Recording

The TUI records every session. `--print`, the SDK, and ACP record only on a remote workspace. Locally they record nothing.

Before a tool call that may change files, Caudra records what the call can change. After the call it records the same scope again and keeps only what changed, with the content before and after. A call that changes nothing leaves no record.

| Call | What its record covers |
| --- | --- |
| File tools | The paths they write |
| Shell lines that only read, such as `git status`, `rg`, or `ls` | Nothing |
| Shell lines whose writes are all literal paths | The paths that `rm`, `mv`, `cp`, `touch`, `mkdir`, `ln`, `tee`, and `sed -i` change, and the targets of output redirects |
| Other shell lines, MCP tools, and plugin tools | The whole session directory |

A glob, a variable, a heredoc, an unlisted option, or any other program sends a shell line to the whole directory. `cp` and `ln` record only the destination, and `mv` or `cp` into a directory records that whole directory. Paths outside the session directory are not recorded. Your own edits, including `!` commands from the composer, get no record of their own.

A whole-directory record cannot tell who made a change. An edit that you or another program make while such a call runs counts as that call's change, and a file revert undoes it. Long background shell commands widen this window.

When a record cannot be made, the reason decides what happens. If the store refuses it, for example because the record would pass a [limit](#limits), the store is full, or the workspace keeps no change records, the call runs without a record. Caudra shows the reason once, and later previews count the call. If the store is busy, the connection to a remote host is lost, or another error occurs, Caudra blocks the call and names the reason, so no change runs without its record. A local store that fails to open with a transient error blocks calls until a later call opens it.

Records are stored in the Git object format under the Caudra state directory, in `workspace-changes/<workspace-hash>/`. Each workspace has one store, shared by every session that works there. Every new record trims the store back to `max_bytes_mb`, dropping the oldest records first, whichever session made them.

Records never include the Caudra state directory. A session whose directory contains the stores, such as one started in your home directory, records nothing, and Caudra says so once.

### Disable change recording

Run `caudra --no-snapshots` to turn change recording off for one process. The flag applies to local and remote workspaces, including TUI, `--print`, SDK, and ACP sessions.

For a persistent setting, add this to your `caudra.toml`:

```toml
[storage.snapshots]
enabled = false
```

`--no-snapshots` overrides the configured value without saving it. Remote sessions read this setting from the client user configuration, not from the remote project.

File revert is unavailable while recording is off. Conversation revert still works. Records already made stay in the store, but a file revert cannot reach back past the point where recording comes back on. An existing file revert can still be unreverted.

### Limits

A whole-directory record walks the session directory. The walk applies the `.gitignore` file of each directory, whether or not the workspace is a Git repository, and does not read `.git/info/exclude` or the global Git ignore file. It stores symbolic links as links and never follows them. It leaves out nested repositories, special files such as sockets and devices, and other filesystems. A path that a call names is recorded even when `.gitignore` ignores it. Records never hold `.git`, `.ssh`, `.workcell`, `.env`, `.npmrc`, `.pypirc`, or `.netrc` at the top of the session directory. A change to anything left out is not recorded, and a revert leaves it alone.

Each whole-directory record walks the tree twice, before and after its call, and reads again only the files that changed.

| Setting | Default | Effect |
| --- | --- | --- |
| `storage.snapshots.enabled` | `true` | `false` turns change recording off |
| `storage.snapshots.max_bytes_mb` | `512` | The most file data one record may cover, and the size each workspace store is trimmed to |
| `storage.snapshots.max_files` | `50000` | The most files one record may cover |
| `storage.snapshots.max_file_bytes_mb` | `100` | Larger files are not stored |

A record that would cover more is refused, and its call runs without a record. Every covered file counts, changed or not. In a session directory over these limits, only file tools and shell lines with literal paths are recorded, and a named directory over them, such as the target of `rm -r`, is refused too. A file over `max_file_bytes_mb` is not stored. When a call changes one, a revert across that call stops with an `unrecorded` conflict.

A value above what the store accepts is lowered to it. Locally that is the default, and a remote host may accept less. Zero is rejected when the configuration loads.

Records keep only the executable bit of a file, as Git does. A reverted file keeps its other permission bits, and a recreated file gets default permissions from your umask. Each file is replaced atomically, but a revert of several files is not atomic as a whole.
