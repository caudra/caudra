use std::fmt::Write;

use caudra_ui::{BUILTIN_COMMANDS, ChatScope};

const MAIN_ONLY_MARK: &str = "Main only";

const ALIASING: &str = r#"## Aliasing commands

Prefer a different name for a command? `caudra.api.run_command` runs any slash command exactly as typing it would, so an alias is a one-line handler in your `init.lua` instead of a reimplementation.

```lua
-- ~/.config/caudra/init.lua
local aliases = {
    { name = "/clear", target = "/new", description = "Alias for /new" },
    { name = "/resume", target = "/sessions", description = "Alias for /sessions" },
}

for _, alias in ipairs(aliases) do
    caudra.api.register_command({
        name = alias.name,
        description = alias.description,
        handler = function()
            local ok, err = caudra.api.run_command(alias.target)
            if not ok then
                caudra.ui.flash("could not run " .. alias.target .. ": " .. err)
            end
        end,
    })
end
```

Both names stay in the palette: aliasing adds a name, it does not rename or hide the original. It works for any command listed above, plus plugin commands and MCP prompts. See [`caudra.api.run_command`](/docs/lua-api/#caudra-api-run_command) for matching and error handling, or [`caudra.ui.action`](/docs/lua-api/#caudra-ui-action) to bind a key instead of a name."#;

const CONTEXT: &str = r#"## Context window

`/context` opens a compact summary of the active context window. Main and each task have separate prompts, tools, and transcripts. The command reports Main when Main is open, or the selected task when its transcript is open. It never adds those windows together. A task restored after restart has no request snapshot until the task is continued.

The summary shows the active model and window, estimated tokens by category, the compaction reserve, and remaining space. `/context all` adds item-level built-in tool, MCP tool, profile, memory, and skill inventories. Both views use the active provider's request projection, so eligible old retained tool results count as compact output-ID markers rather than their full canonical text.

`/tools` covers the tool array on its own. It lists every built-in and MCP tool with its state, its token cost, and the rule behind that state, and it is the only view that shows tools turned off by configuration. See [Tools](/docs/tools/) for the lists that decide those states.

`/skills` does the same for skills. It lists each skill with its file, its scope, and whether its body is already in the window, then every candidate directory with the state directory precedence gave it. It reads the disk when it opens, so it works before the first request. See [Skills](/docs/skills/#where-skills-live).

Token counts are estimates. Deferred MCP definitions and memory or skill bodies stay on demand, and opening either report does not load them. The compact MCP catalog, memory tag index, and skill name and description list count when present. Full definitions and bodies count after the agent loads them.

`/usage` is the cumulative view. It totals provider-reported tokens and priced spend for completed calls across the session, and its global view shows lifetime spend. See [Context](/docs/context/) for how requests are assembled and [Token Economy](/docs/token-economy/#lifetime-spend) for the spending ledger."#;

const LOGS: &str = r#"## Logs

`/logs` opens the structured log Caudra writes for every run. It reads only the rows on screen plus a small buffer, so the modal opens at the same speed on a 5 KB file and a 5 MB one.

New records arrive while the modal is open. Press `f` to pause that and read a fixed view, then `f` again to resume. Scrolling up pauses on its own, and jumping to the end resumes.

Press `l` to cycle the minimum level, or click the level in the footer. Press `Enter` to expand the selected record into every field, its spans, and the raw JSON, `y` to copy the record as shown, and `Y` to copy the stored line.

A record is drawn with every field it has, so a wide one runs past the right margin. Press `w` to wrap it onto as many rows as it needs, or use the arrow keys to pan across it and `Home` to come back. The hint row offers the arrows only while something is out there to reach, so their absence means the record already fits.

Press `Tab` to keep only the records sharing the selected one's narrowest id, which is its tool call, then its request, then its session. That id lands in the filter field, so it can be widened or cleared like anything else typed there. A record carrying no id says so rather than filtering to nothing.

Press `/` to filter. Each term matches as a subsequence, so `tolcal` finds `tool_call`, and a term is compared against the message, the target, the level, and each field and span value on its own. Space separates terms, and a record has to match all of them, so `provider retry` finds a retry from the provider. Filtering reads backward through the rotated files, and the footer says when it reached the oldest one or stopped at the scan limit.

A line the parser cannot read, such as a panic backtrace, is shown as it was written and treated as an error so a filter never hides it.

Prompt text and tool input are not written to the log unless you opt in. See [Telemetry](/docs/telemetry/#privacy). Run `caudra logs` for the same records outside the TUI, and see [Logging](/docs/logging/) for the file, its rotation, and the level."#;

const GOALS: &str = r#"## Completion goals

`/goal <condition>` asks Caudra to keep working until the conversation contains evidence that the condition is met. One goal can be active per session, and a new condition replaces the current one. Conditions are limited to 4,000 characters.

At the end of each natural work turn, a separate model call evaluates the condition against a private copy of the transcript. The evaluator has no tools and its messages do not enter the conversation. A met goal clears itself. An unmet goal adds hidden guidance and starts another work turn. A genuinely impossible goal stops with the evaluator's reason and clears itself.

Run `/goal-model` to choose the evaluator. `/goal model` is also accepted as an alias. Default tries the global Fast preset, then the active provider's weak model, and finally the current conversation model. Fast, Balanced, and Best use their global exact-model preset when assigned, otherwise the matching tier from the active provider. Selecting an exact model may use another provider. Explicit selections report an error instead of silently falling back when unavailable or disallowed.

The evaluator choice is saved globally in the `model.roles` row of Caudra's SQLite state database and applies across sessions. The same Goal mode is available from `/model` with `Tab`. Press uppercase `R` in that mode to restore Default.

Run `/goal` without arguments to open the status panel. It shows the condition, evaluator, elapsed time, evaluation count, spend, and latest reason. The footer shows a compact indicator while a goal is active, and clicking that indicator opens the panel.

Use `/goal-clear` to stop early. `/goal clear` remains an alias, and `stop`, `off`, `reset`, `none`, and `cancel` are also accepted after `/goal`, without regard to case.

Goal state belongs to the session. An active goal survives resume with its condition, evaluation count, spend, elapsed time, and latest reason, and completed status survives too, while `/new` clears them. A resumed goal is status rather than a trigger: it waits for your next message and is evaluated at the end of that turn. Normal permissions still apply, so unattended goals need rules or YOLO mode that already permit the required tools.

Evaluator spend is recorded under the `goal` purpose in the lifetime ledger, billed to whichever provider served the evaluator. See [Lifetime spend](/docs/token-economy/#lifetime-spend).

Caudra defers evaluation while tracked background agents are running and starts a hidden check-in after they finish. Worker compaction can still run, but evaluator calls never compact or alter history.

Eight automatic continuations are allowed in one query. When that safety cap or the configured turn limit is reached, Caudra returns control with the goal still active. Send another message to resume. Evaluator errors also leave the goal active. Authentication, billing, context-limit, and unavailable-model errors clear it when retrying cannot recover.

One-shot headless mode accepts the same form:

```bash
caudra --print --prompt '/goal tests pass and cargo clippy is clean'
```

Headless mode waits for tracked background agents before evaluating. An impossible condition, evaluator failure, continuation cap, or turn limit produces an error result."#;

const STASH: &str = r#"## Stash

A prompt you are not ready to send does not have to block the composer. `/stash` (`Ctrl+X s`) moves the draft out of the way, `/stash-pop` (`Ctrl+X p`) brings the newest one back, and `/stash-list` opens the full list.

The stash keeps the whole composer, so pasted text keeps its `[Pasted N lines]` pill and attached images come back with the draft. Entries are stored in the `input.stash` row of Caudra's SQLite state database, capped at 50, and shared across every session and project. That makes the stash a way to carry a prompt from one project to another. Each entry records the directory it came from, and the list shows that name next to its age.

`/stash-pop` and the list both refuse to restore into a composer that already holds a draft. Stash the current one first, then restore. In the list, `Enter` restores an entry and removes it, and `Ctrl+D` twice deletes without restoring.

Stashing is for drafts you do not want to send yet. To line up prompts Caudra should send on its own, use the queue instead. See [Queue and Steering](/docs/queue/)."#;

const TASKS: &str = r#"## Tasks

Each `task` subagent has a separate transcript. Open the task picker with `/tasks` or `Ctrl+X a`, click the task count above the input, or click a task call in the main chat. Click `[< Main]` in a task's status bar to return. The picker also lists Main and supports previewing every transcript.

An input box appears while the focused task is running. Press Enter to queue guidance for its next turn boundary. Pending guidance stays visible above the input until the subagent consumes it. Task transcripts survive session reloads, and later `task` calls can continue one by passing its `task_id`.

That input box is a full composer. Typing `/` opens the palette, `Ctrl+S` inserts a file path, `Ctrl+X e` edits the draft in your editor, and `Ctrl+V` attaches an image to the guidance. A custom `/project:` or `/user:` command expands its template and steers the focused task rather than the main session.

Commands that reach the main session's turn or history have no task equivalent, so `/compact`, `/model`, `/system-prompt`, `/workflow`, `/btw`, the `/goal` family, and MCP prompts are drawn dimmed and report their scope when run. Return to Main to use them. `/context`, `/tools`, `/skills`, `/queue`, `/review`, and the stash commands already follow the focused transcript."#;

fn write_row(out: &mut String, name: &str, description: &str, scope: ChatScope) {
    let scope = match scope {
        ChatScope::Any => "",
        ChatScope::MainOnly => MAIN_ONLY_MARK,
    };
    writeln!(
        out,
        "| `{name}` | {} | {scope} |",
        description.replace('|', "\\|")
    )
    .unwrap();
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
    writeln!(
        out,
        "Commands marked {MAIN_ONLY_MARK} act on the main session's turn or history. They stay listed while a task transcript is open, drawn dimmed, and report their scope rather than running. See [Tasks](#tasks)."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Command | Description | Scope |").unwrap();
    writeln!(out, "|---------|-------------|-------|").unwrap();
    for cmd in BUILTIN_COMMANDS {
        write_row(&mut out, cmd.name, cmd.description, cmd.scope);
    }

    writeln!(out).unwrap();
    writeln!(out, "## Sessions").unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Sessions run concurrently. `/new` starts a fresh session while the old one keeps working in the background, and `/sessions` shows the live status of each (working, needs input, idle) so you can jump between them. When a background session finishes or needs input, Caudra flashes a note in the status bar. `/rename` renames the current session; in the session picker, `Ctrl+N` / `Ctrl+R` / `Ctrl+D` create, rename, and delete."
    )
    .unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{STASH}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{TASKS}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{GOALS}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "{CONTEXT}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "{LOGS}").unwrap();
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
        "- **`/thinking`**: extended thinking. Optional arg: `off`, `adaptive`, an effort level (`minimal` … `max`), or a token budget number. The level is remembered across restarts. Config: `always_thinking` overrides the remembered level."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/fast`**: Anthropic fast mode (Opus only; ignored on other models). Config: `always_fast = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/workflow`**: expose workflow mode to custom Lua tool descriptions and handlers. Native `python_execution` remains isolated. Config: `always_workflow = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/view`**: cycle the transcript through auto, compact, and expanded. Auto is the default: every card falls back to a single row except the newest one, which stays open until a newer card replaces it. Compact draws every tool call as one row; expanded gives each its own card. Only calls that changed nothing can be hidden, so writes, edits, and shell commands stay open in every mode. An open card shows as much of its body as `ui.tool_output_lines` allows for that tool; clicking shows all of it, and clicking again puts it back. A card you opened yourself stays open as the transcript grows. `task` and `batch` keep their child rows throughout, and each child answers the same question its own card would: a child that changed something draws its body within that tool's `ui.tool_output_lines` budget, and every other child folds to its row until you click it. The choice is remembered across restarts."
    )
    .unwrap();
    writeln!(
        out,
        "- **Plan / build**: not a slash command. Press `Tab` in the input to toggle plan mode (plan-file writes only). Caudra opens in plan mode, and a resumed session reopens in the mode it was left in."
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
        "Your commands come from two places, and each place reads exactly one directory:"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "1. User: the first of `~/.config/caudra/commands/`, `~/.claude/commands/`, `~/.config/opencode/commands/` that exists"
    )
    .unwrap();
    writeln!(
        out,
        "2. Project: walking from the current working directory up to the nearest `.git` root, at each level the first of `.caudra/commands/`, `.claude/commands/`, `.opencode/commands/` that exists"
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "The `.claude` and `.opencode` directories are there so commands you already wrote for other agents keep working. A compatibility directory next to a `.caudra/commands/` is never read, even for a name only it defines. An empty `~/.config/caudra/commands/` counts as existing, so it switches the compatibility directories off."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "A command written for another agent loads as-is. Caudra reads `name`, `description` and `argument-hint` from the frontmatter and ignores the rest, and it substitutes `$ARGUMENTS` only, so OpenCode positional parameters, shell injection and `@file` references stay literal text."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(
        out,
        "Levels still combine. Later sources override earlier ones when the command **name** matches (the stem of the file, or `name` in frontmatter). Because the walk goes cwd → … → git root, a command at the **repository root overrides** the same name found only under a nested cwd. Project commands override user commands. Palette names are `/project:<name>` or `/user:<name>` depending on which scope won."
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
