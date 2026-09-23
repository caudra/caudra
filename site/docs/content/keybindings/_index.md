+++
title = "Keybindings"
weight = 9
[extra]
group = "Reference"
+++

# Keybindings

`Ctrl+X` is the leader. It acts as a prefix: press it, then press the chord's second key. Nothing happens until that second key arrives, and `Esc` cancels. Hold the leader for a moment and a panel lists every chord available where you are.

Leader chords are written as two keys below, and every one of them is reachable on any terminal: Caudra ships no `Alt` defaults, because macOS routes Option through the input method and never reports it as Alt.

## Focus

`PageUp`, `PageDown`, `Home`, and `End` act on whatever holds the keyboard. While you are typing they belong to the composer, so `Home` and `End` move the text cursor and the page keys scroll a draft too tall to fit. When the draft fits, a page key scrolls the transcript and hands it the focus, so `Home` and `End` then reach the top and bottom of the chat.

Typing anything takes the focus back, and so does `Esc`. Clicking the transcript gives it the focus, and clicking the composer returns it. The wheel scrolls whatever the pointer is over and leaves the focus where it is. `Ctrl+U`, `Ctrl+Y`, `Ctrl+E`, `Ctrl+G`, and `Ctrl+B` scroll the transcript wherever the focus sits, and an open modal claims all four navigation keys for itself.

Anywhere a scrollbar is shown it can be dragged. Press the thumb and the surface follows the pointer, press the track anywhere else and the thumb jumps there and stays held. Hold `Alt` while dragging to cover an eighth of the distance, which is what makes a long transcript landable. Dragging the transcript bar shows which message the thumb is on.

A modal whose lines run wider than the screen wears a second bar along its bottom border, and drags the same way. `Shift+Left` and `Shift+Right` move it by a column of a table at a time, and a sideways wheel over the modal moves it too. The bar appears only while there is something off the edge to reach.

That bar carries an arrow at each end, and pressing one moves half a screen; an arrow dims once its direction is spent. The arrows are the way across on a phone: Android terminals send no sideways wheel at all, so a swipe that way reports nothing, and a tap is the only gesture left. They are deliberately easy to hit, so a press just above one still counts.

Holding `Alt` while turning the wheel scrolls four times as far. A middle-click anchors the view and scrolls it on its own, faster the further you then move the pointer from the mark, until you middle-click again or touch anything else. `ui.scrollbar` set to `false` hides the bars and with them the drag.

## General

| Key | Action |
|-----|--------|
| `Ctrl+C` | Quit / clear input (copies instead when text is selected) |
| `Ctrl+D Ctrl+D` | Exit |
| `Ctrl+P` | Command palette |
| `Ctrl+X` | Leader: lists the chords below, then runs the one you press |
| `F1` / `Ctrl+X ?` | Show keybindings |
| `Ctrl+F` | Search messages |
| `Ctrl+X y` | Copy last reply as markdown |
| `Ctrl+X r` | Review the last reply |
| `Ctrl+S` / `Ctrl+X f` | File picker |
| `Ctrl+O` / `Ctrl+X o` | Open the plan in the workbench |
| `Ctrl+X t` | Toggle plan / todo panel |
| `Ctrl+X a` | Open tasks |
| `Ctrl+X k` | Open the workflow inspector |
| `Ctrl+X l` | Browse sessions |
| `Ctrl+X n` | Start a new session |
| `Ctrl+X v` | Toggle compact / expanded transcript |
| `Ctrl+X s` | Stash the current prompt |
| `Ctrl+X p` | Restore the newest stashed prompt |
| `Ctrl+X m` | Model picker |
| `Ctrl+Z` | Suspend process (Unix only) |
| `Shift+Left` / `Shift+Right` | Pan a modal too wide for the screen left / right |
| `Ctrl+X w` | Open the workbench |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `Shift+Enter` / `Ctrl+Enter` / `Ctrl+J` | Newline |
| `Tab` | Toggle BUILD/PLAN mode |
| `Ctrl+T` / `Shift+Tab` | Cycle reasoning effort |
| `/command` | Open command palette |
| `Ctrl+W` / `Ctrl+Backspace` | Delete the word or path component before the cursor |
| `Ctrl+←` / `Ctrl+→` | Move word left / right |
| `Ctrl+Del` | Delete the word or path component after the cursor |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+A` | Select the whole draft |
| `Ctrl+C` | Copy selection (clears the draft when nothing is selected) |
| `Shift+Delete` | Cut selection |
| `Ctrl+Z` / `Ctrl+Y` | Undo / redo the draft |
| `Home` / `End` | Start / end of line or transcript |
| `PageUp` / `PageDown` | Page the draft or the transcript |
| `Ctrl+U` | Scroll half page up |
| `Shift+Left` / `Shift+Right` | Pan a wide diagram left / right |
| `Ctrl+E` | Jump to end of line |
| `Ctrl+G` | Scroll to top |
| `Ctrl+B` | Scroll to bottom |
| `Ctrl+Q` / `Ctrl+X q` | Pop queue |
| `Esc Esc` | Rewind |
| `Ctrl+X e` | Edit the prompt in the workbench |

## Pasted Text

| Key | Action |
|-----|--------|
| `Enter` | Insert newline |
| `Ctrl+A` | Select the whole text |
| `Ctrl+C` / `Shift+Delete` / `Ctrl+V` | Copy, cut or paste the selection |
| `Ctrl+Z` / `Ctrl+Y` | Undo or redo an edit |
| `Ctrl+W` / `Ctrl+Backspace` | Delete the word or path component before the cursor |
| `Ctrl+S` | Save pasted text |
| `Esc` | Cancel editing |

## Review

| Key | Action |
|-----|--------|
| `↑` / `↓` | Move the caret through the passage |
| `Shift+↑` / `Shift+↓` / `Shift+←` / `Shift+→` | Select part of the passage |
| `Ctrl+A` | Select the whole passage |
| `Ctrl+C` | Copy the selection |
| `Ctrl+W` / `Ctrl+Backspace` | Delete the word or path component before the cursor |
| `Enter` | Write a note on the selection |
| `e` / `d` | Edit or delete the note under the cursor |
| `n` / `p` | Jump between notes |
| `Ctrl+S` | Send notes to the prompt |

## While Streaming

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate input history |
| `Esc Esc` | Cancel agent |
| `Enter` | Send prompt next |
| `Ctrl+X g` | Guide current run |
| `Ctrl+X x` | Stop and replace current run |

## Form

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate options |
| `Enter` | Select option |
| `Esc` | Close |

## Pickers

| Key | Action |
|-----|--------|
| `↑` / `↓` | Navigate |
| `Enter` | Select |
| `Esc` | Close |
| `Type` | Filter |
| `PageUp` / `PageDown` | Scroll page up / down |
| `Home` / `End` | First / last item |
| `Ctrl+U` | Scroll page up |

## Sandbox Manager

| Key | Action |
|-----|--------|
| `Ctrl+S` | Validate and save sandbox defaults; export preview saves as a new file |
| `Tab` / `Shift+Tab` | Move focus between sandbox list and form fields (never insert a tab) |
| `Enter` | Inspect/edit; confirmations default to Keep, not Accept |
| `Ctrl+Enter` | Apply to draft, or preview a live action for separate confirmation |
| `F2` | Choose provider, image, policy or purpose-store credential reference |
| `Esc` | Close, not cancel operations; retain live drafts; offer Save/Discard for configuration |
| `1` / `2` / `3` / `4` | Switch Instances, Profiles, Images, Providers when not editing text |
| `/` | Search the sandbox master list |
| `n` / `d` / `Delete` | Profiles: new, duplicate or stage deletion; Instances: d detaches, Delete reviews deletion |
| `g` / `t` | Browse and edit reusable Network or Transfer policies from Profiles |
| `i` / `x` | Import a strict configuration draft or preview a reference-only export |
| `c` / `r` / `a` | Compare baseline/draft/external file, reload, or save as a new private file |
| `Ctrl+Z` / `Ctrl+Y` | Undo/redo sandbox field text; paste and mouse selection use the shared editor |
| `a` / `u` / `p` / `e` | Instances: review Attach, Resume, Pause or Extend |
| `v` / `r` / `z` | Profiles: Create VM; Instances: Reconcile or explicitly Cancel create |
| `h` / `k` | Doctor; Providers: edit lifecycle credential in its purpose store |
| `i` / `b` / `g` / `l` | Images: approved offline Import, Build, GC or Inspect |
| `g` / `F4` / `F6` | Live network preview/apply; Test rules (no probe); discard action draft |

See [Managed Sandboxes](/docs/sandboxes/#tui-manager) for instance actions and [image forms](/docs/sandboxes/#images-and-template-catalog) for the host picker and approved probe. The [Transfer panel](/docs/sandboxes/#tui-transfer-review) has separate controls. Escape there requests cancellation and waits for cleanup, unlike closing a lifecycle action.

## Workbench

| Key | Action |
|-----|--------|
| `Esc` / `Ctrl+X w` | Back to the transcript |
| `Ctrl+B` | Show or hide the sidebar |
| `Ctrl+X -` / `Ctrl+X =` | Narrow / widen the sidebar |
| `Ctrl+X 1` / `Ctrl+X 2` / `Ctrl+X 3` | Explorer / source control / search |
| `Tab` / `Shift+Tab` | Leave the sidebar for the editor |
| `Ctrl+P` | Open a file by name |
| `F5` | Reread the tree and the repository |
| `Ctrl+X Enter` | Send the file or selection to the composer |
| `Ctrl+X .` | Open the context menu for the row or tab under the cursor |

## Context-Specific

Some pickers add extra bindings on top of the defaults:

| Context | Key | Action |
|---------|-----|--------|
| Queue | `Shift+Up` / `Shift+Down` | Move item up / down |
| Queue | `Enter` | Edit item |
| Queue | `d` / `Delete` | Delete item |
| Queue | `m` | Move unsent item to Main |
| Queue | `g` | Guide current run |
| Queue | `n` | Move prompt to Up next |
| Queue | `b` | Toggle send together |
| Queue | `.` | Open item actions |
| Queue | `r` | Replace current run |
| Commands | `Tab` | Complete command |
| Model Picker | `R` | Clear job binding |
| Session Picker | `Ctrl+N` | New session |
| Session Picker | `F2` | Move current session |
| Session Picker | `F3` | Migrate directory sessions |
| Session Relocation | `Ctrl+O` | Enter a custom destination directory |
| Session Relocation | `Ctrl+R` | Change relocation source or destination selection |
| Session Relocation | `Space` | Toggle the selected historical project usage row in bulk confirmation |
| Session Picker | `Ctrl+R` | Rename session |
| Session Picker | `Ctrl+G` | Generate session title |
| Session Picker | `Ctrl+D` | Delete session (press twice) |
| Stash Picker | `Ctrl+D` | Delete stash entry (press twice) |
| Workflow Inspector | `p` | Pause the selected run |
| Workflow Inspector | `r` | Resume the selected run |
| Workflow Inspector | `s` | Stop the selected run |
| Workflow Inspector | `Tab` / `Shift+Tab` | Next or previous section |
| Workflow Inspector | `1-4` | Jump to a section |
| Workflow Inspector | `Left` / `Right` | Focus the run list or the section |
| Workflow Inspector | `Enter` | Open the row under the cursor: a phase's agents, a scratch file, or a call's prompt and result |
| Workflow Inspector | `t` | Open the transcript of the agent under the cursor |
| Workflow Inspector | `o` | Open the script the selected run executed |
| Workflow Inspector | `e` | Copy the whole run as markdown, every prompt and result included |
| Workflow Inspector | `y` | Copy the visible section |
| Workflow Inspector | `/` | Filter the run list |
| Workflow Catalog | `Enter` | Launch a trusted workflow, or trust an untrusted one |
| Workbench Explorer | `Ctrl+X h` | Show hidden files |
| Workbench Explorer | `C` | Fold the tree back to its top level |
| Workbench Editor | `Ctrl+S` | Save the active file |
| Workbench Editor | `Ctrl+R` | Discard edits and take what is on disk |
| Workbench Editor | `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| Workbench Editor | `Ctrl+F` / `Ctrl+G` | Find in file / go to line |
| Workbench Editor | `F3` / `Shift+F3` | Next / previous match, with or without the find bar |
| Workbench Editor | `Ctrl+C` / `Ctrl+V` | Copy / paste |
| Workbench Editor | `Shift+Delete` / `Ctrl+X x` | Cut the selection |
| Workbench Editor | `Ctrl+A` | Select the whole buffer |
| Workbench Editor | `Ctrl+K` | Delete to the end of the line |
| Workbench Editor | `Ctrl+W` / `Ctrl+Backspace` | Delete the word or path component before the cursor |
| Workbench Editor | `Ctrl+X z` | Wrap long lines onto more rows |
| Workbench Editor | `Ctrl+PageUp` / `Ctrl+PageDown` | Previous / next tab |
| Workbench Editor | `Ctrl+X k` | Close the active tab |
| Workbench Editor | `Ctrl+X v` | Show a Markdown file rendered, or its source again |
| Workbench Source Control | `Space` | Stage or unstage the file, folder, or whole section |
| Workbench Source Control | `D` | Open the diff, or the commit under the cursor |
| Workbench Source Control | `X` | Discard changes (press twice) |
| Workbench Source Control | `T` | Switch the change sections between tree and flat |
| Workbench Source Control | `Ctrl+X ↑` / `Ctrl+X ↓` | Shrink / grow the section the cursor is in |
| Workbench Search | `Enter` | Run the search, then open the file at the match |
| Workbench Search | `Ctrl+X i` | Move between the query and the file globs |
| Workbench Search | `Ctrl+X c` | Match case |
| Workbench Search | `Ctrl+X w` | Match whole words |
| Workbench Search | `Ctrl+X r` | Read the query as a regular expression |

## Context Inheritance

Child contexts inherit their parent's bindings and add their own.

- **Pickers** is the base for: Rewind Picker, Theme Picker, Model Picker, Queue, Commands, Search, File Picker, Stash Picker, Session Picker, Session Relocation, Workflow Inspector, Workflow Catalog
- **Workbench** is the base for: Workbench Explorer, Workbench Editor, Workbench Source Control, Workbench Search

## Overriding Keybindings

Plugins and `init.lua` can rebind keys at runtime with `caudra.keymap.set` and `caudra.keymap.del`. The tables above are the built-in defaults. An override on the same key wins, unless a modal or overlay is open (help, plan form, permission prompt).

Precedence, high to low:

1. **Suspend** (`Ctrl+Z`, Unix). Always wins, non-remappable.
2. **Modal and overlay keys.** An open modal or picker consumes its keys first, so they cannot be shadowed while open.
3. **Lua overrides** from `caudra.keymap.set`. Last set wins; binding the same key twice warns.
4. **Built-in defaults.** An override on the same key shadows them; `caudra.keymap.del` lifts the override so the default returns. Suspend is the only binding outside this layer, so every key is remappable except `Ctrl+Z`.

Only single-key bindings can be overridden. Multi-key combinations and non-key rows (like `Type` to filter) cannot.

The `/help` modal and the splash show default labels, not live overrides, but pressing the key still runs the override.

### Recovering from a bad keymap

If an override leaves Caudra stuck (a rebound `Ctrl+C`, a modal that won't close, a plugin that throws on load), boot without user `init.lua`:

```bash
caudra --no-plugins
```

Skips user `init.lua` files (global and project). The Lua host stays up and every built-in tool is native, so tools still work. `permissions.toml`, custom commands, and env files load as usual.

The default keymap lives in Rust, not Lua, so `--no-plugins` never drops it.

## Shell and images

These are input conventions, not remappable key rows:

- Prefix a line with `!` to run a shell command yourself (5 minute timeout). Use `!!` to hide the command and its output from the agent.
- `Ctrl+V` pastes an image from the clipboard into the prompt when the model supports vision. You can also paste image file paths.
- Text pastes with at least 3 lines or more than 150 characters appear as compact tokens. Focus one and press `Enter`, or click it, to edit the complete pasted text.
