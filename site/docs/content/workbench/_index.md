+++
title = "Workbench"
weight = 37
[extra]
group = "Guides"
+++

# Workbench

The workbench combines a file explorer, tabbed editor, source control, project search, and sandbox file transfers. It takes over the terminal beside the transcript, so you can read a file, stage a change, or point the agent at an exact line without leaving Caudra.

Press `Ctrl+X w` to open it, or run `/workbench`. `Esc` or `Ctrl+X w` goes back to the transcript. `Esc` drops a live selection in the editor first, so leaving from one takes a second press. Leaving an active Transfer view waits for cancellation and cleanup. The session keeps running while the workbench is on screen.

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
| Click a file | Show it in a preview tab |
| Click a file twice | Keep the tab it was shown in |
| Click a folder | Expand or collapse it |
| Click a tab | Switch to it |
| Click the `×` on a tab | Close it |
| Middle-click a tab | Close it |
| Click the `⋮` on a row or a tab | Open its [context menu](#context-menu) |
| Right-click a row or a tab | Open its [context menu](#context-menu) |
| Click `FILES`, `GIT`, or `FIND` | Switch the sidebar view |
| Click `FOLD` | Fold the explorer back to its top level |
| Click `TREE` or `FLAT` | Switch how source control lists paths |
| Click `Aa`, `ab`, or `.*` | Turn that search toggle on or off |
| Click a source control header | Fold or unfold that section |
| Click `+` or `-` on a source control row | Stage or unstage that path |
| Click `↗` on a file | Open the file instead of its diff |
| Click `↺` twice on an unstaged file | Discard its changes |
| Click `↺` on an unstaged folder or header | Ask before discarding everything it lists |
| Drag a source control header | Resize the section above it |
| Drag the divider | Resize the sidebar |
| Wheel over a pane | Scroll that pane |
| Alt and the wheel | Scroll four times as far |
| Sideways wheel over the buffer | Pan the text left or right |
| Drag a scrollbar | Move that pane to anywhere in its content |
| Click a scrollbar track | Jump there, and keep dragging from that point |

In the buffer, click to place the cursor and drag to select. A drag that runs past the top or bottom edge scrolls the buffer and keeps the selection growing. Click twice to take the word under the pointer, three times to take the whole line. Letting go puts whatever is selected on the system clipboard, so `Ctrl+C` is a second way rather than the only one. `Shift+Delete` and `Backspace` also work on that selection.

In source control, in search, and in the `Ctrl+P` file picker, one click does whatever `Enter` would have done to that row, so a folder or a section folds and everything else opens for good.

Whatever the pointer rests on is highlighted, so you can see what a click would hit. A row that is already selected is left as it is.

A pane whose content runs past its bottom gives up its last column to a scrollbar, so you can see how much is off screen. Panes that fit keep their full width, and setting `ui.scrollbar` to `false` turns the bars off here as it does everywhere else.

The bar is also a handle. Press the thumb and drag it and the pane follows. Press the track anywhere else and the thumb jumps there and stays held, so one press covers both the coarse move and the fine one. Sliding off the column does not drop the drag. A short track over a long file is hundreds of lines per row, so hold `Alt` while dragging to cover an eighth of the distance and land on the line you wanted. The editor shows which line the thumb is on while you drag it.

Dragging a bar moves the window and leaves the cursor where it was, the way scrolling a buffer does. The next arrow key pulls the window back to it.

### Touch

Touch handling widens the strip a press on a scrollbar may land on to three columns, so a fingertip can reach a bar that is still painted one column wide. It also moves the content one line per wheel event rather than `ui.mouse_scroll_lines`, which keeps the text under your finger as you drag.

Caudra turns it on by itself when it finds Termux around it, which works when Caudra runs on the phone. Reaching Caudra on another machine by SSH is the more common way to use a phone as a terminal, and no Termux variable survives that trip, so set `ui.touch` to `on` in the config on the host you connect to. Set it to `off` for a Bluetooth mouse in Termux.

Termux reports a finger drag as wheel events, so a thumb cannot be dragged with a finger. Tap the track and the pane jumps to that point. Text selection stays with the terminal, where a long press already handles it, so Caudra starts no selection of its own while touch handling is on.

## Context menu

Every explorer row and every tab carries a `⋮` at its left. Click it to open a menu for that row or tab. The right button does the same from anywhere on the row or the tab, and `Ctrl+X .` opens the menu over whatever the cursor is already on. Opening a tab menu leaves the file on screen alone.

Arrow keys walk the menu, `Home` and `End` jump to either end, `Enter` takes the highlighted item, and `Esc` closes it. A press off the panel closes the menu and is swallowed, so the press that dismisses a menu does not also act on what is under it.

An explorer row offers Open, New File, New Folder, Copy Path, Copy Relative Path, Send to Composer, Rename, and Delete. A folder has no Open, because pressing one expands it. Something new lands inside the folder you asked from, and beside the file you asked from.

Rename, New File, and New Folder ask for a name in the status row, and a rename starts from the name the path already has. `Enter` commits and `Esc` cancels. A name that is empty, holds a path separator, or is already taken is refused with the reason in the status row, and what you typed stays in the box to be corrected.

Delete is permanent, with no trash to recover from. The question says what goes: the path itself, and for a folder the number of paths under it, counting the ones the repository ignores. `D` deletes and `C` cancels. Tabs on a deleted path close with it. A tab holding unsaved edits stays open and raises a conflict instead, so the work is still there to save somewhere else.

A tab offers Close, Close Others, Close to the Right, Close Saved, and Close All, along with Keep Open for a preview tab, Save for a tab with unsaved edits, and Reveal in Explorer. A diff tab has no file behind it, so it offers none of the items that name one.

A batch close stops at the first tab with unsaved changes and asks about that tab alone. Answering the question carries on through the rest. Cancelling it, or a save that fails, drops the tabs still queued behind it.

## Explorer

Arrow keys walk the tree. `Right` and `Enter` expand a directory or open a file, `Left` collapses it or jumps to the parent. `C` folds the whole tree back to its top level, and so does `FOLD` in the header. `Ctrl+X h` shows dotfiles. One click of the [mouse](#mouse) opens a file as a [preview](#editor), two keep it.

A rule runs down each level of indent, so a name three folders deep says which folder it belongs to. Folders holding nothing but one folder share a row, the way `src/main/java` reads as one step rather than three. Opening that row opens the last folder on it.

Rows carry three marks. On the left, a `⋮` that opens the [context menu](#context-menu) for that path. It sits ahead of the indent rules, so the marks line up in one column however deep the tree runs. On the right, the source control letter for that path: `M` modified, `A` added, `D` deleted, `U` untracked, `!` conflicted. The name takes the colour of that letter, and a folder you have closed carries the loudest mark under it, so a conflict is visible before you open anything. Files that changed on disk while the workbench was open are marked as well, which in practice means the ones Caudra wrote.

Paths the repository ignores are listed and drawn back, so a build directory is somewhere you can still look without it competing with your source.

In a remote workspace, the top-level entries arrive first and the rest of the tree fills in as listing pages arrive. Listing reads file metadata rather than file contents, so a large binary or source map cannot prevent other files from appearing. A partial listing keeps the entries already received and reports that the result is incomplete. Opening or changing a file performs its own checks.

Remote pages use a bounded, short-lived metadata inventory rather than walking the tree again for every page. They describe the tree when that inventory was captured. Watches and manual refresh reconcile later changes, while saves and other mutations still check the selected file's current revision.

`Ctrl+P` opens a fuzzy file picker over the whole project. Type part of a path, `Enter` opens it. Before you type anything it lists your other open tabs first, most recent before the rest, so `Ctrl+P` then `Enter` goes back to the file you came from. The project is walked once and reused, and walked again after `F5`, after `Ctrl+X h`, or when a file appears or disappears on disk.

## Editor

Tabs sit above the buffer, each with a `⋮` to open its [context menu](#context-menu) and a `×` to close it. One click in the explorer puts a file in a preview tab, whose title is italic. The next preview takes that tab over, so reading your way through a tree leaves one tab behind rather than twenty. Clicking the file again, opening it with `Enter`, or typing in it keeps the tab for good. `Ctrl+PageUp` and `Ctrl+PageDown` cycle them, `Ctrl+X k` closes the active one. When more tabs are open than the strip can hold, it scrolls to keep the active one in view and marks the end it cut off with `‹` or `›`. A tab with unsaved changes asks before it goes, whichever way you close it: **Save** writes the file and closes, **Don't Save** throws the edits away, **Cancel** keeps the tab. `Left` and `Right` walk the answers, `Enter` takes the highlighted one, `Esc` cancels, and `S`, `D`, and `C` pick one outright. A save that fails leaves the tab open with the reason in the status row.

Editing is ordinary: type to insert, `Enter` and `Backspace` do what they look like, `Shift` with a motion selects, `Ctrl+A` selects the buffer. `Ctrl+C` copies and `Shift+Delete` cuts to the system clipboard, `Ctrl+V` puts back what the workbench last took, and a terminal paste inserts at the cursor. `Ctrl+K` deletes to the end of the line. `Ctrl+Z` and `Ctrl+Y` undo and redo, grouped so a run of typing undoes in one press.

Cut is `Shift+Delete`, not `Ctrl+X`. `Ctrl+X` is Caudra's leader everywhere, including here, so that the chords above stay reachable while text is selected. `Ctrl+X x` cuts as well, for a terminal that keeps `Shift+Delete` for itself. Cut with nothing selected does nothing rather than deleting the character at the cursor.

`Ctrl+S` saves. `Ctrl+F` opens find in file, then `Enter` or `Down` goes to the next match and `Shift+Enter` or `Up` to the previous one. `F3` and `Shift+F3` do the same thing without the bar open, so `Esc` puts the buffer back and you can keep walking the matches. `Ctrl+G` goes to a line number.

`Ctrl+X z` wraps long lines onto more rows instead of leaving them off to the right. A wrapped line breaks between words, keeps its number in the gutter on the first row only, and ignores the sideways pan, because the pane is already showing every column it has. The workbench remembers the setting per project.

Files the editor cannot take still open. Binaries, files over 8 MiB, and files that are not valid UTF-8 open read-only, and the status row says which of the three it is.

### When the agent writes the same file

The workbench watches the project while it is open. A file that changes on disk reloads in place when its tab has no unsaved edits, keeping the cursor where it was.

A tab with unsaved edits keeps them and raises a conflict instead. The status row says so, and `Ctrl+R` resolves it by throwing the buffer away and taking what is on disk. `Ctrl+S` refuses a stale save instead of overwriting the other writer's changes. Copy any edits you want to keep before reloading.

Bursts of writes settle before the panes react, so a build or a `git checkout` costs one refresh rather than one per file.

The watch covers the project and stops while the workbench is closed. A file outside the project, such as the [plan](#plans-memory-notes-and-prompt-drafts), reloads when the agent's own tool call writes it, under the same rules. Reopening the workbench rereads every tab whose file changed while it was closed, and so does opening a file that already has a tab.

If a remote watch cannot start, browsing and manual refresh remain available. The status row reports that live updates are unavailable while bounded retries attempt to reconnect. Closing the workbench stops those retries. A successful watch installation refreshes the listing to cover changes that happened before the watch was ready.

## Rendered Markdown

A Markdown file can be read the way the transcript shows Markdown, with headings, lists, tables, and code blocks drawn out and the syntax gone. `Ctrl+X v` switches the active tab between its source and its rendered view. The tab's [context menu](#context-menu) does the same with `Show Rendered` or `Show Source`, whichever view is hidden. Files ending in `.md` or `.markdown` have a rendered view, and so does the [prompt draft](#plans-memory-notes-and-prompt-drafts).

The rendered view is for reading. The arrow keys, `PageUp`, `PageDown`, `Home`, `End`, the wheel, and the scrollbar move it, and typing flashes that it is read-only. Each view opens at the same point through the document that the other one showed, and the cursor stays where you left it in the source. A reload, a theme change, or a resize shows up in the view at once. On a file, `Ctrl+X Enter` sends a mention without a line number, because the rendered view has no cursor.

`Ctrl+X v` switches the transcript view everywhere else, and only belongs to the rendered view while the workbench is open.

## Source control

`Ctrl+X 2` shows three stacked sections, with the current branch in the sidebar header.

If the selected remote directory is not inside a Git repository, this pane shows `Not a Git repository`. File browsing and editing remain available. Inaccessible or damaged repositories still report an error.

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
│ ● ▾ 4f2a1c fix  │
│ │   src/        │
│ │     one.rs  M │
│ ◉ ▸ 91be07 merg │
│ │○▸ 0cd334 wip  │
└─────────────────┘
```

Each header carries a chevron, a title, and how many rows the section holds. `Space` on a header stages or unstages every path the section lists, so one press empties `CHANGES` into `STAGED CHANGES`.

Resting the pointer on a row brings up what it can do, to the left of the git letter. Whichever button the pointer is over is lit, so the row says what a click would press. A row under `CHANGES` offers `+` to stage it, and one under `STAGED CHANGES` offers `-` to unstage it. Both work on a folder as well as a file, and on the header, where they cover the whole section. A file also offers `↗`, which opens the file itself rather than the diff a plain click gives you. An unstaged row offers `↺`, which discards its changes. That one is destructive, so it takes two clicks: the first says what it is about to throw away in the status row, and anything else you click cancels it. A folder and the section header offer it too, covering everything they list, and those raise a dialog that says how many files it would reach.

Drag a header to resize the section above it, and click one to fold that section away. A section with nothing in it is drawn folded. The bottom open section takes whatever room is left, so resizing the terminal moves that border and leaves the others where you dragged them. `Ctrl+X ↑` and `Ctrl+X ↓` do the same from the keyboard.

All three sections nest paths as folders. A folder with one child is joined onto its parent, so `src/main/rust` is one row rather than three. `T` switches them to flat full paths, and the `TREE` or `FLAT` label on the right of the sidebar header does the same with the mouse.

Rows in `GRAPH` carry a rail glyph: `●` for a commit on the chain of first parents, `◉` for a merge, and `│○` for a commit a merge brought in. The rail is one lane wide, so it says where a commit sits against the first-parent chain rather than drawing every branch.

`Enter` on a commit opens it into the paths it changed, laid out as a tree under it and marked with the same git letters the change sections use. `Enter` on one of those paths opens the diff for that path alone, read against the commit's first parent. Only the path you open is read, so a commit touching hundreds of files costs one tree walk to list and one file to show. The listing stops at 100 paths and says so.

A row has width for a subject and no more, so a commit whose message says more than its subject carries a `¶` after it. `D` on a commit opens the whole message as a read-only tab: the hash, who wrote it and when, the parents it was built on, the message laid out as it was written, and the paths it touched. The tab is ordinary text, so `Ctrl+F` searches it and `Ctrl+C` copies from it. Long lines are left alone rather than rewrapped, because a message may hold a code fence or a table; `Ctrl+X z` wraps them if you would rather read it that way.

Clicking a closed commit does both at once: it lists the paths and opens the message. Clicking an open one only closes it, and leaves whatever tab you are reading where it is.

Any number of commits can stay open at once, and a folder folded under one commit stays open under another. A commit's message, a commit's diff, and the working tree's diff of the same path are all separate tabs.

| Key | Action |
|-----|--------|
| `Space` | Stage or unstage the file, the folder, or the whole section |
| `Enter` | Open the diff, or fold what the cursor is on |
| `D` | Open the diff, or the whole message when the cursor is on a commit |
| `X` | Discard changes, twice to confirm |
| `T` | Switch between tree and flat |
| `Left` / `Right` | Fold and unfold a folder, a commit, or a section |
| `Ctrl+X ↑` / `Ctrl+X ↓` | Resize the section the cursor is in |

Diffs open as read-only tabs. Discarding is destructive. On one file it asks for a second press of the same key or a second click of `↺`. On a folder or a whole section it raises a dialog instead, because a row says how many paths it covers and not what is in them.

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

## Transfer

After attaching to a sandbox, select `TRANSFER` or press `Ctrl+X 4`. The editor area shows local files on the left and sandbox files on the right. Your open editor tabs keep their contents and cursor positions. Local-only sessions and direct Workcell connections do not expose this view.

Choose an existing absolute local directory with `L` and an existing workspace-relative sandbox directory with `S`, then Compare. Each pane header shows its root. The sandbox root is relative to the exposed Workcell workspace, not the guest filesystem. A remote conversation path is never used as a local directory automatically. Changing the root pair discards the old review and requires a fresh comparison.

The comparison is one tree, laid out like the Explorer. Each row holds the same relative path in both panes. An entry that exists on one side only leaves a dim `·` on the other, so the rows stay aligned. Folders expand in place. Expanded folders stay open when you compare again, and the cursor returns to the same path or its nearest remaining parent.

Comparison checks content and executable metadata rather than choosing the newest timestamp. Marks on the right of each row:

| Mark | Meaning |
|------|---------|
| `≠` | Content differs |
| `+` | Present only on this side |
| `!` | File and folder type conflict |
| `?` | Not determined, for example on an incomplete side |
| `● n` | Folder with n changed entries below it |

Identical rows carry no mark. A folder with matching names can still contain changed or unreadable files. Ignored, protected, excluded, symlink, nested-repository, special and unsupported entries are dimmed and labelled with a word badge. An expanded folder that is empty, left out or not fully scanned ends with a note row that explains why. A partly listed folder shows the entries that were listed first.

| Note | Meaning |
|------|---------|
| Ignored by .gitignore | `I` includes ignored files |
| Protected | Never transferred |
| Excluded by a transfer pattern | Matched a configured exclude |
| Symbolic link, nested repository or special file | Never followed or transferred |
| Unsupported | Never transferred |
| Not fully scanned | A scan limit or listing error stopped here. `Enter` compares this folder |
| Contents unknown | That side is incomplete. `Enter` compares this folder |
| Empty folder | Can still be transferred |

`I` compares again with `.gitignore` filtering turned off for this Transfer session. Protected names and configured excludes stay excluded. The choice is not saved and resets when Transfer reopens. A review made under one choice cannot be approved under the other.

Large trees can stop at a scan limit. Folders that were listed still compare by content. Entries present on one side only stay undetermined while either side is partial. When the Workcell inventory cap truncates a side, file diffs are refused too. A banner names the incomplete side. `Enter` on a "Not fully scanned" or "Contents unknown" note re-roots both sides to the folder that holds the note. That folder must exist on both sides. `Backspace` restores the previous root pair, including one changed with `L` or `S`.

Select files or folders with `Space`, then press `U` to review an upload or `D` to review a download. Without a selection, the row under the cursor is reviewed. Folder selection includes eligible descendants and empty directories. Skipped entries remain visible. Review requires a complete comparison. One review carries at most 128 paths, counting every file and new folder under a selected folder. A larger selection is refused whole rather than copying only part of it. File/directory type conflicts cannot be overwritten by a transfer.

Empty-directory creation requires negotiated directory-publication support. Local directory publication currently requires Linux and private staging on the same filesystem as the destination, outside the transferred tree. When that support is unavailable, file transfers remain available.

`Enter` on a changed file opens a read-only diff drawn like the editor's diff tabs, with line numbers and syntax colours. `←` and `→` pan a wide diff. Each side reads at most 64 KiB, and a badge marks truncation. Binary files show size, digest and kind per side. Inspecting a file does not authorize copying it. The review lists new files, overwrites and directory creation, and native permissions still apply on both ends.

| Key | Action |
|-----|--------|
| `↑` / `↓`, `j` / `k`, `PgUp` / `PgDn`, `Home` / `End` | Move |
| `→` / `Enter` | Expand a folder or open a file's diff. On a note row, run its action |
| `←` | Collapse, or go to the parent |
| `C` | Collapse all |
| `Tab` / `Shift+Tab` | Focus the local or sandbox pane. A narrow terminal shows one pane at a time |
| `Space` | Select or deselect a file or folder |
| `U` / `D` | Review an upload or download of the selection, or of the row under the cursor |
| `A` | Approve the displayed executable review |
| `=` / `F5` | Compare again |
| `I` | Include or exclude ignored files for this session |
| `F` | Show changes only |
| `L` / `S` | Edit the local or sandbox root |
| `Ctrl+U` | Clear the root being edited |
| `Backspace` | Restore the previous root pair |
| `O` | Show the last transfer report |
| `Q` | Reconcile recorded uncertain outcomes. This needs the connection a comparison opens, so compare again if it has closed |
| `X` | Stop the running operation |
| `Esc` | Close the prompt or panel, then leave Transfer |

The sidebar sums up the changes, what the selection would carry each way, and the last transfer report. `Ctrl+B`, `Ctrl+X -` and `Ctrl+X =` hide and resize it as in the other views. `Ctrl+P` leaves Transfer for the Explorer and opens quick open once cleanup ends.

The mouse follows the Explorer. Click a row to move the cursor, click its marker or double-click a folder to expand it, and click the check column to select. Double-click a file to open its diff. Toolbar buttons run their action, and clicking a pane header edits that root. While a root is being edited, the toolbar waits for it to be confirmed or cancelled.

While a transfer connection holds the editing guard, ordinary editing, saves, source-control mutations and composer submission are blocked across Caudra sessions. Save or discard dirty buffers and let active work settle before connecting. Leaving Transfer cancels its worker and waits for cleanup before returning to editing. External editors and processes are outside this guard.

Transfers do not delete destination-only files or synchronize automatically. Earlier confirmed operations remain applied after a later failure. Unknown outcomes require Reconcile, which queries recorded status without retrying publication. See [reviewed transfers](/docs/sandboxes/#reviewed-file-transfers) for permissions, exclusions, CLI commands and recovery guarantees.

## Sending a reference to the agent

`Ctrl+X Enter` puts what you are looking at into the composer as a [mention](/docs/context/), then closes the workbench so you can finish the sentence. `Send to Composer` in a row's [context menu](#context-menu) does the same for a path you have not opened.

- From the explorer or source control: `@path/to/file`
- From a search result: `@path/to/file:L42`
- From the editor with the cursor on a line: `@path/to/file:L42`
- From the editor with a selection: `@path/to/file:L10-L20`

This is the fastest way to say "look at this" without typing the path or the line numbers. When you send the prompt, Caudra reads the lines a mention names and puts them in the request, so the agent starts with them rather than calling `file_read`.

## Plans, memory notes, and prompt drafts

Caudra opens its own text here as well. Each tab is named for what it holds, and the status row says the rest: `Plan · <file>`, `Memory · <note>`, or `Prompt · <chat>`.

- **The plan.** `Ctrl+O` or `Ctrl+X o` opens it in a tab named `Plan`, and so does `Ctrl+O` on the plan form. `Ctrl+S` saves your edits. Implementing reads the plan from disk, so while its tab holds unsaved edits, Implement brings the tab back and asks you to save first. The plan form stays up behind the workbench and leaves every key to it. A plan the agent has not written yet has nothing to open, and the status bar says so.
- **Memory notes.** `Enter` on a note in `/memory`, or a click on a note the `memory` tool shows in the transcript, opens it in a tab named after the note.
- **The prompt draft.** `Ctrl+X e` in the composer opens what you have typed in a tab named `Prompt`, with every folded paste spelled out. `Ctrl+S` puts the draft back in the composer and keeps the workbench open. `Ctrl+X Enter` puts it back and closes the workbench, so you can send it.

The plan and the notes live in the state directory, outside the project. When the agent rewrites the plan or saves a note, its tab reloads from that tool call, under the same rules as a [watched file](#when-the-agent-writes-the-same-file).

In a [remote workspace](/docs/remote-workspaces/) the plan and the notes stay on your machine, as documents addressed by reference rather than by path. They open here all the same, and `Ctrl+S` writes them back to where they came from. The plan's status row shows the start of its reference where a local plan shows its file name. A save only goes through when nothing else has written the document since its tab last read it. Otherwise the status bar says the document changed, your edits stay in the tab, and `Ctrl+R` replaces them with the newer copy.

A prompt draft is not a file. Each chat has its own, and a draft only goes back to the composer it came from. Saving one while another chat is in front flashes a reminder, and the tab keeps your edits until you switch back. Leaving the workbench drops a draft with nothing unsaved in it, so the next `Ctrl+X e` starts again from the composer. A draft with unsaved edits stays, and `Ctrl+X e` brings it back as you left it.

## What is remembered

Open tabs, the active tab, the sidebar view, its width, whether hidden files are shown, and how the source control sections were sized and folded are stored per project directory. Reopening the workbench in the same checkout restores them once per run. Files that have since been deleted are skipped. Diff tabs are not restored, because they are built from the repository rather than read from a path. Prompt drafts and the plan and notes of a remote workspace are not stored either, and a restored plan tab is named after its file.

Two clones of the same repository keep separate layouts.

## Limits

The workbench is an editor beside an agent, not a replacement for your own. There is no language server, no completion, no split panes, and no modal editing. Search does not replace. Staging is per file. The rendered view is read-only and has no selection, so copy from the source.

## Keys

Every binding is in the [keybindings reference](/docs/keybindings/) under Workbench, Workbench Explorer, Workbench Editor, Workbench Source Control, Workbench Search, and Workbench Transfer. `Ctrl+Z` suspends Caudra everywhere else, and the workbench takes it for undo while it is open.
