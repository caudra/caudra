+++
title = "Keybindings"
weight = 9
[extra]
group = "Reference"
+++

# Keybindings

On macOS, some bindings use Option or Fn keys instead (run `/help` for exact keybindings).

## General

| Key | Action |
|-----|--------|
| `Ctrl+C` | Quit / clear input |
| `Ctrl+D Ctrl+D` | Exit |
| `Ctrl+P` | Command palette |
| `Ctrl+H` | Show keybindings |
| `Ctrl+F` | Search messages |
| `Alt+C` | Copy last reply as markdown |
| `Alt+A` | Review the last reply |
| `Ctrl+S` | File picker |
| `Ctrl+O` | Open plan in editor |
| `Ctrl+T` | Toggle plan / todo panel |
| `Ctrl+X` | Open tasks |
| `Alt+P` | Browse sessions |
| `Alt+V` | Toggle compact / expanded transcript |
| `Alt+T` | Stash the current prompt |
| `Alt+R` | Restore the newest stashed prompt |
| `Ctrl+M` / `Alt+M` | Model picker |
| `Alt+E` | Open the workbench |

## Editing

| Key | Action |
|-----|--------|
| `Enter` | Submit prompt |
| `Shift+Enter` / `Ctrl+Enter` / `Ctrl+J` / `Alt+Enter` | Newline |
| `Tab` | Toggle BUILD/PLAN mode |
| `Shift+Tab` | Cycle reasoning effort |
| `/command` | Open command palette |
| `Ctrl+W` | Delete word backward |
| `Alt+←` / `Alt+→` | Move word left / right |
| `Ctrl+A` | Jump to start of line |
| `Home` / `End` | Jump to start/end of line |
| `Ctrl+U` / `PageUp` | Scroll half page up |
| `PageDown` | Scroll half page down |
| `Shift+Left` / `Shift+Right` | Pan a wide diagram left / right |
| `Ctrl+E` | Jump to end of line |
| `Ctrl+G` / `Ctrl+Home` | Scroll to top |
| `Ctrl+B` / `Ctrl+End` | Scroll to bottom |
| `Ctrl+Q` | Pop queue |
| `Esc Esc` | Rewind |
| `Alt+O` | Edit input in external editor |

### macOS-specific

| Key | Action |
|-----|--------|
| `Ctrl+Del` / `⌥Del` | Delete word forward |
| `Ctrl+K` | Delete to end of line |

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
| `Alt+S` | Guide current run |
| `Alt+X` | Stop and replace current run |

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
| `Esc` / `Alt+E` | Back to the transcript |
| `Ctrl+B` | Show or hide the sidebar |
| `Alt+-` / `Alt+=` | Narrow / widen the sidebar |
| `Alt+1` / `Alt+2` / `Alt+3` | Explorer / source control / search |
| `Tab` / `Shift+Tab` | Leave the sidebar for the editor |
| `Ctrl+P` | Open a file by name |
| `F5` | Reread the tree and the repository |
| `Alt+Enter` | Send the file or selection to the composer |

## Context-Specific

Some pickers add extra bindings on top of the defaults:

| Context | Key | Action |
|---------|-----|--------|
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
| Workbench Explorer | `Ctrl+H` | Show hidden and ignored files |
| Workbench Editor | `Ctrl+S` | Save the active file |
| Workbench Editor | `Ctrl+R` | Discard edits and take what is on disk |
| Workbench Editor | `Ctrl+Z` / `Ctrl+Y` | Undo / redo |
| Workbench Editor | `Ctrl+F` / `Ctrl+G` | Find in file / go to line |
| Workbench Editor | `F3` / `Shift+F3` | Next / previous match, with or without the find bar |
| Workbench Editor | `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste |
| Workbench Editor | `Ctrl+A` | Select the whole buffer |
| Workbench Editor | `Ctrl+K` | Delete to the end of the line |
| Workbench Editor | `Alt+Z` | Wrap long lines onto more rows |
| Workbench Editor | `Alt+Left` / `Alt+Right` | Previous / next tab |
| Workbench Editor | `Alt+W` | Close the active tab |
| Workbench Source Control | `Space` | Stage or unstage the file, folder, or whole section |
| Workbench Source Control | `D` | Open the diff, or the commit under the cursor |
| Workbench Source Control | `X` | Discard changes (press twice) |
| Workbench Source Control | `T` | Switch the change sections between tree and flat |
| Workbench Source Control | `Alt+Up` / `Alt+Down` | Shrink / grow the section the cursor is in |
| Workbench Search | `Enter` | Run the search, then open the file at the match |
| Workbench Search | `Alt+I` | Move between the query and the file globs |
| Workbench Search | `Alt+C` | Match case |
| Workbench Search | `Alt+M` | Match whole words |
| Workbench Search | `Alt+R` | Read the query as a regular expression |

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
