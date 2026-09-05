+++
title = "Workbench"
weight = 37
[extra]
group = "Guides"
+++

# Workbench

The workbench is a file explorer, tabbed editor, source control view, and project search, laid out the way an IDE lays them out. It takes over the terminal beside the transcript, so you can read a file, stage a change, or point the agent at an exact line without leaving Caudra.

Press `Alt+E` to open it, or run `/workbench`. `Esc` or `Alt+E` goes back to the transcript. The session keeps running while the workbench is on screen.

## Layout

A sidebar on the left, tabs and a buffer on the right, one status row along the bottom.

```
┌─ Explorer ──────┬─ a.txt × ─ b.rs ──────────────┐
│  sub/           │  1  one                       │
│  a.txt        M │  2  two                       │
│  b.rs         U │  3  three                     │
├─────────────────┴───────────────────────────────┤
│ a.txt          Ln 3, Col 1  LF  Alt+Enter send  │
└─────────────────────────────────────────────────┘
```

`Ctrl+B` hides the sidebar. `Alt+-` and `Alt+=` change its width, and you can drag the divider with the mouse. On a terminal too narrow for both panes the sidebar drops out and the editor keeps the room.

`Tab` and `Shift+Tab` move between the sidebar and the editor. `Alt+1`, `Alt+2`, and `Alt+3` switch the sidebar to the explorer, source control, or search, and put the cursor there.

## Explorer

Arrow keys walk the tree. `Right` and `Enter` expand a directory or open a file, `Left` collapses it or jumps to the parent. `Ctrl+H` shows hidden and ignored files.

Rows carry two marks. On the right, the source control letter for that path: `M` modified, `A` added, `D` deleted, `U` untracked, `!` conflicted. Files that changed on disk while the workbench was open are marked as well, which in practice means the ones Caudra wrote.

`Ctrl+P` opens a fuzzy file picker over the whole project. Type part of a path, `Enter` opens it.

## Editor

Tabs sit above the buffer. `Alt+Left` and `Alt+Right` cycle them, `Alt+W` closes the active one. A tab with unsaved changes refuses to close and says so in the status row.

Editing is ordinary: type to insert, `Enter` and `Backspace` do what they look like, `Shift` with a motion selects, `Ctrl+A` selects the buffer. `Ctrl+C` and `Ctrl+X` copy and cut to the system clipboard, `Ctrl+V` puts back what the workbench last took, and a terminal paste inserts at the cursor. `Ctrl+K` deletes to the end of the line. `Ctrl+Z` and `Ctrl+Y` undo and redo, grouped so a run of typing undoes in one press.

`Ctrl+S` saves. `Ctrl+F` opens find in file, then `Enter` or `Down` goes to the next match and `Shift+Enter` or `Up` to the previous one. `Ctrl+G` goes to a line number.

Files the editor cannot take still open. Binaries, files over 8 MiB, and files that are not valid UTF-8 open read-only, and the status row says which of the three it is.

### When the agent writes the same file

The workbench watches the project while it is open. A file that changes on disk reloads in place when its tab has no unsaved edits, keeping the cursor where it was.

A tab with unsaved edits keeps them and raises a conflict instead. The status row says so, and `Ctrl+R` resolves it by throwing the buffer away and taking what is on disk. Saving over the other writer is the other way out, and `Ctrl+S` does that.

Bursts of writes settle before the panes react, so a build or a `git checkout` costs one refresh rather than one per file.

## Source control

`Alt+2` lists staged and unstaged changes, with the current branch in the sidebar header.

| Key | Action |
|-----|--------|
| `Space` | Stage or unstage the selected file |
| `D` or `Enter` | Open the diff |
| `X` | Discard changes, twice to confirm |
| `L` | Switch between the change list and the commit log |

Diffs open as read-only tabs. Discarding is destructive and asks for a second press of the same key.

Caudra reads and writes the repository directly with [gix](https://github.com/GitoxideLabs/gitoxide), so nothing here shells out to `git`. Staging works on whole files. Hunk-level staging, committing, and branch operations are not part of this view, so use the terminal or ask the agent.

A directory outside a repository says so rather than failing.

## Search

`Alt+3` searches file contents across the project. The pane has a query field, a comma-separated glob field for narrowing by path, and three toggles.

| Key | Action |
|-----|--------|
| `Enter` | Run the search, then open the file at the match |
| `Alt+I` | Move between the query and the glob field |
| `Alt+C` | Match case |
| `Alt+M` | Match whole words |
| `Alt+R` | Read the query as a regular expression |

The first `Enter` runs the search. Once results are current, `Enter` opens the selected row: a file heading opens the top of the file, a match opens that line. Editing the query or a toggle makes the results stale again, so the next `Enter` searches.

The walk respects `.gitignore` and skips `.git`, binaries, and files above the size limit. Results stream in as they are found and stop at 5000 matches, which the pane says out loud rather than pretending the list is complete.

## Sending a reference to the agent

`Alt+Enter` puts what you are looking at into the composer as a file reference, then closes the workbench so you can finish the sentence.

- From the explorer or source control: `@path/to/file`
- From a search result: `@path/to/file:L42`
- From the editor with the cursor on a line: `@path/to/file:L42`
- From the editor with a selection: `@path/to/file:L10-L20`

This is the fastest way to say "look at this" without typing the path or the line numbers.

## What is remembered

Open tabs, the active tab, the sidebar view, its width, and whether hidden files are shown are stored per project directory. Reopening the workbench in the same checkout restores them once per run. Files that have since been deleted are skipped. Diff tabs are not restored, because they are built from the repository rather than read from a path.

Two clones of the same repository keep separate layouts.

## Limits

The workbench is an editor beside an agent, not a replacement for your own. There is no language server, no completion, no split panes, and no modal editing. Search does not replace. Staging is per file.

## Keys

Every binding is in the [keybindings reference](/docs/keybindings/) under Workbench, Workbench Explorer, Workbench Editor, Workbench Source Control, and Workbench Search. `Ctrl+Z` suspends Caudra everywhere else, and the workbench takes it for undo while it is open.
