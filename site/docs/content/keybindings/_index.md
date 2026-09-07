+++
title = "Keybindings"
weight = 9
[extra]
group = "Reference"
+++

# Keybindings

`Ctrl+X` is the leader. It acts as a prefix: press it, then press the chord's second key. Nothing happens until that second key arrives, and `Esc` cancels. Hold the leader for a moment and a panel lists every chord available where you are.

Leader chords are written as two keys below, and every one of them is reachable on any terminal: Caudra ships no `Alt` defaults, because macOS routes Option through the input method and never reports it as Alt.

## General

| Key | Action |
|-----|--------|
| `Ctrl+C` | Quit / clear input |
| `Ctrl+D Ctrl+D` | Exit |
| `Ctrl+P` | Command palette |
| `Ctrl+X` | Leader: lists the chords below, then runs the one you press |
| `F1` / `Ctrl+X ?` | Show keybindings |
| `Ctrl+F` | Search messages |
| `Ctrl+X y` | Copy last reply as markdown |
| `Ctrl+X r` | Review the last reply |
| `Ctrl+S` / `Ctrl+X f` | File picker |
| `Ctrl+O` / `Ctrl+X o` | Open plan in editor |
| `Ctrl+X t` | Toggle plan / todo panel |
| `Ctrl+X a` | Open tasks |
| `Ctrl+X l` | Browse sessions |
| `Ctrl+X n` | Start a new session |
| `Ctrl+X v` | Toggle compact / expanded transcript |
| `Ctrl+X s` | Stash the current prompt |
| `Ctrl+X p` | Restore the newest stashed prompt |
| `Ctrl+X m` | Model picker |
| `Ctrl+Z` | Suspend process (Unix only) |
| `Ctrl+X w` | Open the workbench |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `Shift+Enter` / `Ctrl+Enter` / `Ctrl+J` | Newline |
| `Tab` | Toggle BUILD/PLAN mode |
| `Ctrl+T` / `Shift+Tab` | Cycle reasoning effort |
| `/command` | Open command palette |
| `Ctrl+W` / `Ctrl+Backspace` | Delete word backward |
| `Ctrl+←` / `Ctrl+→` | Move word left / right |
| `Ctrl+Del` | Delete word forward |
| `Ctrl+K` | Delete to end of line |
| `Ctrl+A` | Jump to start of line |
| `Home` / `End` | Jump to start/end of line |
| `Ctrl+U` / `PageUp` | Scroll half page up |
| `PageDown` | Scroll half page down |
| `Shift+Left` / `Shift+Right` | Pan a wide diagram left / right |
| `Ctrl+E` | Jump to end of line |
| `Ctrl+G` / `Ctrl+Home` | Scroll to top |
| `Ctrl+B` / `Ctrl+End` | Scroll to bottom |
| `Ctrl+Q` / `Ctrl+X q` | Pop queue |
| `Esc Esc` | Rewind |
| `Ctrl+X e` | Edit input in external editor |

## Pasted Text

| Key | Action |
|-----|--------|
| `Enter` | Insert newline |
| `Ctrl+S` | Save pasted text |
| `Esc` | Cancel editing |

## Review

| Key | Action |
|-----|--------|
| `j` / `k` / `g` / `G` | Move the row cursor |
| `v` | Extend the passage |
| `Enter` | Write a note on the passage |
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
| `Ctrl+U` | Scroll page up |

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
| Commands | `Tab` | Complete command |
| Model Picker | `Tab` / `Shift+Tab` | Switch model purpose |
| Model Picker | `R` | Reset model purpose |
| Session Picker | `Ctrl+N` | New session |
| Session Picker | `Ctrl+R` | Rename session |
| Session Picker | `Ctrl+G` | Name session with a small model |
| Session Picker | `Ctrl+D` | Delete session (press twice) |
| Stash Picker | `Ctrl+D` | Delete stash entry (press twice) |
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
| Workbench Editor | `Ctrl+X z` | Wrap long lines onto more rows |
| Workbench Editor | `Ctrl+PageUp` / `Ctrl+PageDown` | Previous / next tab |
| Workbench Editor | `Ctrl+X k` | Close the active tab |
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

- **Pickers** is the base for: Rewind Picker, Theme Picker, Model Picker, Queue, Commands, Search, File Picker, Stash Picker, Session Picker
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
