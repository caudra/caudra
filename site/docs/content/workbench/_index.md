+++
title = "Workbench"
weight = 37
[extra]
group = "Guides"
+++

# Workbench

The workbench is a file explorer, tabbed editor, source control view, and project search, laid out the way an IDE lays them out. It takes over the terminal beside the transcript, so you can read a file, stage a change, or point the agent at an exact line without leaving Caudra.

Press `Ctrl+X w` to open it, or run `/workbench`. `Esc` or `Ctrl+X w` goes back to the transcript. `Esc` drops a live selection in the editor first, so leaving from one takes a second press, while `Ctrl+X w` always leaves at once. The session keeps running while the workbench is on screen.

## Layout

A sidebar on the left, tabs and a buffer on the right, one status row along the bottom.

```
┌─────────────────┬───────────────────────────────┐
│ FILES GIT FIND  │  a.txt ×  ●b.rs ×             │
│  sub/           │  1  one                       │
│  a.txt        M │  2  two                       │
│  b.rs         U │  3  three                     │
├─────────────────┴───────────────────────────────┤
│ a.txt        Ln 3, Col 1  LF  Ctrl+X Enter send │
└─────────────────────────────────────────────────┘
```

`Ctrl+B` hides the sidebar. `Ctrl+X -` and `Ctrl+X =` change its width, and you can drag the divider with the mouse. On a terminal too narrow for both panes the sidebar drops out and the editor keeps the room.

`Tab` and `Shift+Tab` move between the sidebar and the editor. `Ctrl+X 1`, `Ctrl+X 2`, and `Ctrl+X 3` switch the sidebar to the explorer, source control, or search, and put the cursor there. The `FILES`, `GIT`, and `FIND` labels in the sidebar header do the same thing with the mouse.

## Mouse

The workbench takes the mouse the way an IDE does.

| Action | Result |
|--------|--------|
| Click a file | Open it |
| Click a folder | Expand or collapse it |
| Click a tab | Switch to it |
| Click the `×` on a tab | Close it |
| Middle-click a tab | Close it |
| Click `FILES`, `GIT`, or `FIND` | Switch the sidebar view |
| Click `TREE` or `FLAT` | Switch how source control lists paths |
| Click `Aa`, `ab`, or `.*` | Turn that search toggle on or off |
| Click a source control header | Fold or unfold that section |
| Click `+` or `-` on a source control row | Stage or unstage that path |
| Click `↗` on a staged file | Open the file instead of its diff |
| Click `↺` twice on an unstaged file | Discard its changes |
| Click `↺` on an unstaged folder or header | Ask before discarding everything it lists |
| Drag a source control header | Resize the section above it |
| Drag the divider | Resize the sidebar |
| Wheel over a pane | Scroll that pane |
| Sideways wheel over the buffer | Pan the text left or right |

In the buffer, click to place the cursor and drag to select. A drag that runs past the top or bottom edge scrolls the buffer and keeps the selection growing. Click twice to take the word under the pointer, three times to take the whole line. Letting go puts whatever is selected on the system clipboard, so `Ctrl+C` is a second way rather than the only one. `Shift+Delete` and `Backspace` also work on that selection.

Source control and search rows follow the explorer, and so do the `Ctrl+P` file picker's: one click does whatever `Enter` would have done to that row, so a folder or a section folds and everything else opens.

Whatever the pointer rests on is highlighted, so you can see what a click would hit. A row that is already selected is left as it is.

A pane whose content runs past its bottom gives up its last column to a scrollbar, so you can see how much is off screen. Panes that fit keep their full width, and setting `ui.scrollbar` to `false` turns the bars off here as it does everywhere else. The bar is a marker rather than a handle: use the wheel or the arrow keys to move.

## Explorer

Arrow keys walk the tree. `Right` and `Enter` expand a directory or open a file, `Left` collapses it or jumps to the parent. `Ctrl+X h` shows hidden and ignored files. One click of the [mouse](#mouse) does the same as `Enter`.

Rows carry two marks. On the right, the source control letter for that path: `M` modified, `A` added, `D` deleted, `U` untracked, `!` conflicted. Files that changed on disk while the workbench was open are marked as well, which in practice means the ones Caudra wrote.

`Ctrl+P` opens a fuzzy file picker over the whole project. Type part of a path, `Enter` opens it. Before you type anything it lists your other open tabs first, most recent before the rest, so `Ctrl+P` then `Enter` goes back to the file you came from. The project is walked once and reused, and walked again after `F5`, after `Ctrl+X h`, or when a file appears or disappears on disk.

## Editor

Tabs sit above the buffer, each with a `×` to close it. `Ctrl+PageUp` and `Ctrl+PageDown` cycle them, `Ctrl+X k` closes the active one. When more tabs are open than the strip can hold, it scrolls to keep the active one in view and marks the end it cut off with `‹` or `›`. A tab with unsaved changes asks before it goes, whichever way you close it: **Save** writes the file and closes, **Don't Save** throws the edits away, **Cancel** keeps the tab. `Left` and `Right` walk the answers, `Enter` takes the highlighted one, `Esc` cancels, and `S`, `D`, and `C` pick one outright. A save that fails leaves the tab open with the reason in the status row.

Editing is ordinary: type to insert, `Enter` and `Backspace` do what they look like, `Shift` with a motion selects, `Ctrl+A` selects the buffer. `Ctrl+C` copies and `Shift+Delete` cuts to the system clipboard, `Ctrl+V` puts back what the workbench last took, and a terminal paste inserts at the cursor. `Ctrl+K` deletes to the end of the line. `Ctrl+Z` and `Ctrl+Y` undo and redo, grouped so a run of typing undoes in one press.

Cut is `Shift+Delete`, not `Ctrl+X`. `Ctrl+X` is Caudra's leader everywhere, including here, so that the chords above stay reachable while text is selected. `Ctrl+X x` cuts as well, for a terminal that keeps `Shift+Delete` for itself. Cut with nothing selected does nothing rather than deleting the character at the cursor.

`Ctrl+S` saves. `Ctrl+F` opens find in file, then `Enter` or `Down` goes to the next match and `Shift+Enter` or `Up` to the previous one. `F3` and `Shift+F3` do the same thing without the bar open, so `Esc` puts the buffer back and you can keep walking the matches. `Ctrl+G` goes to a line number.

`Ctrl+X z` wraps long lines onto more rows instead of leaving them off to the right. A wrapped line breaks between words, keeps its number in the gutter on the first row only, and ignores the sideways pan, because the pane is already showing every column it has. The workbench remembers the setting per project.

Files the editor cannot take still open. Binaries, files over 8 MiB, and files that are not valid UTF-8 open read-only, and the status row says which of the three it is.

### When the agent writes the same file

The workbench watches the project while it is open. A file that changes on disk reloads in place when its tab has no unsaved edits, keeping the cursor where it was.

A tab with unsaved edits keeps them and raises a conflict instead. The status row says so, and `Ctrl+R` resolves it by throwing the buffer away and taking what is on disk. Saving over the other writer is the other way out, and `Ctrl+S` does that.

Bursts of writes settle before the panes react, so a build or a `git checkout` costs one refresh rather than one per file.

## Source control

`Ctrl+X 2` shows three stacked sections, with the current branch in the sidebar header.

```
┌─────────────────┐
│ FILES GIT FIND  │
│ ▾ STAGED CHANGES│
│   a.txt       M │
│ ▾ CHANGES     3 │
│   src/          │
│     one.rs    M │
│     two.rs    U │
│ ▾ GRAPH         │
│ ● 4f2a1c fix …  │
│ ◉ 91be07 merge  │
│ │○ 0cd334 wip   │
└─────────────────┘
```

Each header carries a chevron, a title, and how many rows the section holds. `Space` on a header stages or unstages every path the section lists, so one press empties `CHANGES` into `STAGED CHANGES`.

Resting the pointer on a row brings up what it can do, to the left of the git letter. A row under `CHANGES` offers `+` to stage it, and one under `STAGED CHANGES` offers `-` to unstage it. Both work on a folder as well as a file, and on the header, where they cover the whole section. A staged file also offers `↗`, which opens the file itself rather than the diff a plain click gives you. An unstaged file offers `↺`, which discards its changes. That one is destructive, so it takes two clicks: the first says what it is about to throw away in the status row, and anything else you click cancels it. A folder and the section header offer it too, covering everything they list, and those raise a dialog that says how many files it would reach.

Drag a header to resize the section above it, and click one to fold that section away. A section with nothing in it is drawn folded. The bottom open section takes whatever room is left, so resizing the terminal moves that border and leaves the others where you dragged them. `Ctrl+X ↑` and `Ctrl+X ↓` do the same from the keyboard.

The two change sections nest paths as folders. A folder with one child is joined onto its parent, so `src/main/rust` is one row rather than three. `T` switches both sections to flat full paths, and the `TREE` or `FLAT` label on the right of the sidebar header does the same with the mouse.

Rows in `GRAPH` carry a rail glyph: `●` for a commit on the chain of first parents, `◉` for a merge, and `│○` for a commit a merge brought in. The rail is one lane wide, so it says where a commit sits against the first-parent chain rather than drawing every branch. `Enter` opens the whole commit as a read-only tab: the message, the author, and the diff against its first parent, up to 100 files.

| Key | Action |
|-----|--------|
| `Space` | Stage or unstage the file, the folder, or the whole section |
| `D` or `Enter` | Open the diff, the commit, or fold what the cursor is on |
| `X` | Discard changes, twice to confirm |
| `T` | Switch the change sections between tree and flat |
| `Left` / `Right` | Fold and unfold a folder or a section |
| `Ctrl+X ↑` / `Ctrl+X ↓` | Resize the section the cursor is in |

Diffs and commits open as read-only tabs. Discarding is destructive. On one file it asks for a second press of the same key or a second click of `↺`. On a folder or a whole section it raises a dialog instead, because a row says how many paths it covers and not what is in them.

Caudra reads and writes the repository directly with [gix](https://github.com/GitoxideLabs/gitoxide), so nothing here shells out to `git`. Staging works on whole files. Hunk-level staging, committing, and branch operations are not part of this view, so use the terminal or ask the agent.

A directory outside a repository says so rather than failing.

## Search

`Ctrl+X 3` searches file contents across the project. The pane has a query field, a comma-separated glob field for narrowing by path, and three toggles.

| Key | Action |
|-----|--------|
| `Enter` | Run the search, then open the file at the match |
| `Ctrl+X i` | Move between the query and the glob field |
| `Ctrl+X c` | Match case |
| `Ctrl+X w` | Match whole words |
| `Ctrl+X r` | Read the query as a regular expression |

The first `Enter` runs the search. Once results are current, `Enter` opens the selected row: a file heading opens the top of the file, a match opens that line. Editing the query or a toggle makes the results stale again, so the next `Enter` searches.

The walk respects `.gitignore` and skips `.git`, binaries, and files above the size limit. Results stream in as they are found and stop at 5000 matches, which the pane says out loud rather than pretending the list is complete.

## Sending a reference to the agent

`Ctrl+X Enter` puts what you are looking at into the composer as a file reference, then closes the workbench so you can finish the sentence.

- From the explorer or source control: `@path/to/file`
- From a search result: `@path/to/file:L42`
- From the editor with the cursor on a line: `@path/to/file:L42`
- From the editor with a selection: `@path/to/file:L10-L20`

This is the fastest way to say "look at this" without typing the path or the line numbers.

## What is remembered

Open tabs, the active tab, the sidebar view, its width, whether hidden files are shown, and how the source control sections were sized and folded are stored per project directory. Reopening the workbench in the same checkout restores them once per run. Files that have since been deleted are skipped. Diff tabs are not restored, because they are built from the repository rather than read from a path.

Two clones of the same repository keep separate layouts.

## Limits

The workbench is an editor beside an agent, not a replacement for your own. There is no language server, no completion, no split panes, and no modal editing. Search does not replace. Staging is per file.

## Keys

Every binding is in the [keybindings reference](/docs/keybindings/) under Workbench, Workbench Explorer, Workbench Editor, Workbench Source Control, and Workbench Search. `Ctrl+Z` suspends Caudra everywhere else, and the workbench takes it for undo while it is open.
