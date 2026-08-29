+++
title = "Sessions, Forks, and Revert"
weight = 35
[extra]
group = "Guides"
+++

# Sessions, forks, and revert

Maki stores conversation history as parent-linked items. User prompts, assistant text, reasoning, tool calls, and tool results are separate items. A session head selects the active path through those items. Moving the head keeps the abandoned path available for unrevert and later forks.

## Message actions

Right-click a message, or hold the left mouse button for half a second, to open Message Actions. Normal left clicks still select text, expand reasoning, and interact with tool output.

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

Retained tool output belongs to one session and is stored under `tool-output/<session-id>/` in the Maki state directory. An output ID can be read or searched only from its owning session. Live sessions have no age-based expiry for these results.

Each retained output is capped at 100 MiB. Deleting a session deletes its retained outputs. A fork copies the outputs referenced by its selected ancestor path and reachable subagent histories into the child session, preserving their opaque IDs there.

If a failed deletion or interrupted write leaves output without a session, output-store startup cleanup removes it after a seven-day grace period. Retained outputs belonging to a live session remain untouched.

## Conversation revert

A conversation revert changes the active head rather than deleting items. Selecting a user prompt lands before that prompt and restores it to the composer. Other item types are inclusive.

Sending a prompt after revert creates a new branch from that point. The abandoned branch remains stored. Unrevert is available until new work commits the new branch.

`Esc Esc` while idle uses the same conversation-only mechanism through the rewind picker.

## File snapshots

Maki creates a session-start snapshot before the first top-level run. It snapshots the current history head before each later run and the resulting head after completion or cancellation. Reusing a head refreshes its pre-run snapshot, so edits made while idle are included. A file restore selects the nearest available snapshot at or before the chosen item. Several parallel tool calls therefore share one safe run checkpoint.

Snapshots are content-addressed with SHA-256 and stored under the Maki state directory in `session-snapshots/<session-id>/<workspace-hash>/`. The object store contains the complete file bytes under their hashes. A checkpoint manifest maps each relative path to its object hash and Unix mode. Unchanged files reuse the same object instead of storing another copy.

Each session workspace has a 512 MiB retention target. Old checkpoint manifests are removed first. The session-start anchor and data needed by an active revert remain available, so protected data can exceed the target.

In a Git worktree, snapshot walks follow Git ignore rules. Outside Git, Maki walks all regular files below the session directory. `.git`, symlinks, special files, and paths outside the session directory are not captured. Changing a path between captured and ignored or symlink state is outside the restore guarantee because manifests cannot distinguish that state from absence.

Restore compares the current file hash and mode with the source snapshot. If a tracked path changed outside the captured run, restore aborts and reports a conflict. Maki does not overwrite it automatically. Conversation-only revert remains available when file restore cannot proceed.

Only paths that differ between the source and target manifests are touched. Files created after the target are deleted, deleted files are recreated, and unrelated files remain in place. Each file replacement is atomic. A restore spanning several files completes through the journal.

Before applying changes, Maki captures the current state of every affected path. Unrevert restores this state before moving the conversation head back. A restore journal records prepare, apply, and verification phases so a later restore can finish an interrupted transaction.

Snapshots cover regular files. They cannot reverse running processes, databases, network calls, Git branches or index state, nested repository state, or commands that changed files outside the session directory. Changes from manual shell commands can appear as conflicts. `/cd` changes the process workspace for every live session and is blocked while any session is busy or has a pending revert.
