use std::fmt::Write;

use maki_ui::BUILTIN_COMMANDS;

use crate::lua_util;

const ALIASING: &str = r#"## Aliasing commands

Prefer a different name for a command? `maki.api.run_command` runs any slash command exactly as typing it would, so an alias is a one-line handler in your `init.lua` instead of a reimplementation.

```lua
-- ~/.config/maki/init.lua
local aliases = {
    { name = "/clear", target = "/new", description = "Alias for /new" },
    { name = "/resume", target = "/sessions", description = "Alias for /sessions" },
}

for _, alias in ipairs(aliases) do
    maki.api.register_command({
        name = alias.name,
        description = alias.description,
        handler = function()
            local ok, err = maki.api.run_command(alias.target)
            if not ok then
                maki.ui.flash("could not run " .. alias.target .. ": " .. err)
            end
        end,
    })
end
```

Both names stay in the palette: aliasing adds a name, it does not rename or hide the original. It works for any command listed above, plus plugin commands and MCP prompts. See [`maki.api.run_command`](/docs/lua-api/#maki-api-run_command) for matching and error handling, or [`maki.ui.action`](/docs/lua-api/#maki-ui-action) to bind a key instead of a name."#;

const GOALS: &str = r#"## Completion goals

`/goal <condition>` asks Maki to keep working until the conversation contains evidence that the condition is met. One goal can be active per session, and a new condition replaces the current one. Conditions are limited to 4,000 characters.

At the end of each natural work turn, a separate model call evaluates the condition against a private copy of the transcript. The evaluator has no tools and its messages do not enter the conversation. A met goal clears itself. An unmet goal adds hidden guidance and starts another work turn. A genuinely impossible goal stops with the evaluator's reason and clears itself.

Run `/goal-model` to choose the evaluator. `/goal model` is also accepted as an alias. Auto preserves the default behavior: use the active provider's weak model, then fall back to the current conversation model when no weak model is available. Strong, Medium, and Weak explicitly select that tier from the active provider. Selecting an exact model may use another provider. Explicit selections report an error instead of silently falling back when unavailable or disallowed.

The evaluator choice is saved globally in `~/.local/state/maki/model-roles` and applies across sessions. You can also open `/model` and press `Ctrl+G` on a row to toggle that exact model as the goal evaluator. Run `/goal-model` and select Auto to restore the default.

Run `/goal` without arguments to open the status panel. It shows the condition, evaluator, elapsed time, evaluation count, spend, and latest reason. The footer shows a compact indicator while a goal is active.

Use `/goal-clear` to stop early. `/goal clear` remains an alias, and `stop`, `off`, `reset`, `none`, and `cancel` are also accepted after `/goal`, without regard to case.

Goal state belongs to the session. Active and completed status survive resume, while `/new` clears them. Normal permissions still apply, so unattended goals need rules or YOLO mode that already permit the required tools.

Maki defers evaluation while tracked background agents are running and starts a hidden check-in after they finish. Worker compaction can still run, but evaluator calls never compact or alter history.

Eight automatic continuations are allowed in one query. When that safety cap or the configured turn limit is reached, Maki returns control with the goal still active. Send another message to resume. Evaluator errors also leave the goal active. Authentication, billing, context-limit, and unavailable-model errors clear it when retrying cannot recover.

One-shot headless mode accepts the same form:

```bash
maki --print '/goal tests pass and cargo clippy is clean'
```

Headless mode waits for tracked background agents before evaluating. An impossible condition, evaluator failure, continuation cap, or turn limit produces an error result."#;

const TASKS: &str = r#"## Tasks

Each `task` subagent has a separate transcript. Open the task picker with `/tasks` or `Ctrl+X`, or click a task call in the main chat. Click `[< Main]` in a task's status bar to return. The picker also lists Main and supports previewing every transcript.

An input box appears while the focused task is running. Press Enter to queue guidance for its next turn boundary. Pending guidance stays visible above the input until the subagent consumes it. Task transcripts survive session reloads, and later `task` calls can continue one by passing its `task_id`."#;

fn write_row(out: &mut String, name: &str, description: &str) {
    writeln!(out, "| `{name}` | {} |", description.replace('|', "\\|")).unwrap();
}

pub fn generate() -> String {
    let mut out = String::new();
    writeln!(out, "+++").unwrap();
    writeln!(out, "title = \"Commands\"").unwrap();
    writeln!(out, "weight = 8").unwrap();
    writeln!(out, "[extra]").unwrap();
    writeln!(out, "group = \"Reference\"").unwrap();
    writeln!(out, "+++").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "# Commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Type `/` in the input box to open the command palette."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## Built-in commands").unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Command | Description |").unwrap();
    writeln!(out, "|---------|-------------|").unwrap();
    for cmd in BUILTIN_COMMANDS {
        write_row(&mut out, cmd.name, cmd.description);
    }
    for cmd in &lua_util::load_builtin_plugin_commands() {
        write_row(&mut out, &cmd.name, &cmd.description);
    }

    writeln!(out).unwrap();
    writeln!(out, "## Sessions").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Sessions run concurrently. `/new` starts a fresh session while the old one keeps working in the background, and `/sessions` shows the live status of each (working, needs input, idle) so you can jump between them. When a background session finishes or needs input, Maki flashes a note in the status bar. `/rename` renames the current session; in the session picker, `Ctrl+N` / `Ctrl+R` / `Ctrl+D` create, rename, and delete."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{TASKS}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{GOALS}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "## Modes and toggles").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "- **`/yolo`**: skip permission prompts for this session (deny rules still apply). The toggle survives a resume, and `--yolo` only sets the starting value. Config: `always_yolo = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/thinking`**: extended thinking. Optional arg: `off`, `adaptive`, an effort level (`minimal` … `max`), or a token budget number. Config: `always_thinking`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/fast`**: Anthropic fast mode (Opus only; ignored on other models). Config: `always_fast = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/workflow`**: let `code_execution` call the `task` tool (and other workflow-only tools) from inside the Python sandbox. Config: `always_workflow = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **Plan / build**: not a slash command. Press `Tab` in the input to toggle plan mode (plan-file writes only)."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/reload`**: rebuild plugins and config without leaving the app."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/btw`**: one-shot side question with no tools and no history pollution."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/memory`**: open the memory file picker (view / edit / delete). See the `memory` tool under [Tools](/docs/tools/)."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "## Custom commands").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "You can define your own slash commands as Markdown files. Empty files are skipped."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Discovery and priority").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Later sources override earlier ones when the command **name** matches (the stem of the file, or `name` in frontmatter):"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "1. User config: `~/.config/maki/commands/` (and legacy `~/.maki/commands/` if present)"
    )
    .unwrap();
    writeln!(out, "2. User third-party: `~/.claude/commands/`").unwrap();
    writeln!(
        out,
        "3. Project dirs, walking from the current working directory up to the nearest `.git` root. At each level: `.maki/commands/`, then `.claude/commands/`"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Because the walk goes cwd → … → git root, a command at the **repository root overrides** the same name found only under a nested cwd. Project commands override user commands. Palette names are `/project:<name>` or `/user:<name>` depending on which scope won."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Skip all of the above with `--no-commands` (see [CLI](/docs/cli/))."
    )
    .unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Metadata").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "You can add optional metadata at the top of the file between `---` lines to set `name`, `description`, and `argument-hint`:"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "```markdown").unwrap();
    writeln!(out, "---").unwrap();
    writeln!(out, "description: Review code for issues").unwrap();
    writeln!(out, "argument-hint: <file>").unwrap();
    writeln!(out, "---").unwrap();
    writeln!(out, "Review $ARGUMENTS and suggest improvements.").unwrap();
    writeln!(out, "```").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "### Arguments").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Use `$ARGUMENTS` in the command body. It gets replaced with whatever you type after the command name. The command is treated as accepting args if the body contains `$ARGUMENTS` or you set `argument-hint`."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "For example, `/project:review main.rs` replaces `$ARGUMENTS` with `main.rs`."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "{ALIASING}").unwrap();

    writeln!(out).unwrap();
    writeln!(
        out,
        "Related: [CLI](/docs/cli/) for shell flags and subcommands, [Skills](/docs/skills/) for on-demand playbooks."
    )
    .unwrap();

    if out.ends_with('\n') {
        out.pop();
    }
    out
}
