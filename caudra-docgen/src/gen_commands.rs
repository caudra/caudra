use std::fmt::Write;

use caudra_ui::{BUILTIN_COMMANDS, BuiltinCommand, ChatScope};

const MAIN_ONLY_MARK: &str = "Main only";

const MODEL_JOBS: &str = r#"## Model jobs

`/model` opens a Jobs overview and model list. Selecting a model there changes Chat. `/goal-model` opens the Goal assignment page directly.

See [Providers](/docs/providers/#model-jobs) for the jobs, picker controls, assignments, automatic resolution, and supply markers."#;

const ALIASING: &str = r#"## Aliasing commands

Prefer a different name for a command? With [Lua plugins](/docs/configuration/#experimental-features) turned on, `caudra.api.run_command` runs any slash command exactly as typing it would, so an alias is a one-line handler in your `init.lua` instead of a reimplementation.

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

`/projection` shows what `/context` counts: the system prompt, the tools, and every message as the provider receives it, with `r` switching to the JSON body the active provider would send. See [Provider request projection](/docs/context/#provider-request-projection).

`/tools` lists every built-in and MCP tool with its state, token cost, and the rule behind that state, including tools turned off by configuration. It previews the selected mode while Main is idle and shows the committed execution mode during an active turn. A task view uses that task's own snapshot. See [Inspect the active window](/docs/context/#inspect-the-active-window) for pending-mode behavior and [Tools](/docs/tools/) for availability rules. `g` cycles the modal through recorded activity for the session, the project, and everything ever run, with call counts, error classes, estimated result tokens, and wall clock per tool. See [Token Economy](/docs/token-economy/#what-the-tools-cost).

`/skills` does the same for skills. It lists each skill with its file, its scope, and whether its body is already in the window, then every candidate directory with the state directory precedence gave it. It reads the disk when it opens, so it works before the first request. See [Skills](/docs/skills/#where-skills-live).

Token counts are estimates. Deferred MCP definitions and memory or skill bodies stay on demand, and opening either report does not load them. The compact MCP catalog, memory tag index, and skill name and description list count when present. Full definitions and bodies count after the agent loads them.

`/usage` is the cumulative view. It totals provider-reported tokens and priced spend for completed calls across the session, and its global view shows lifetime spend. Both scopes break the work down per model, and per provider once more than one served it, with a `hit` column for [cache hit rate](/docs/token-economy/#cache-hit-rate). See [Context](/docs/context/) for how requests are assembled and [Token Economy](/docs/token-economy/#lifetime-spend) for the spending ledger."#;

const DOCS: &str = r#"## Docs

`/docs` opens this manual inside Caudra, as it was when your binary was built. The first time, it shows the contents, grouped as on the docs home page. After that it reopens where you left it. `/docs permissions` opens a page, and `/docs permissions#plan-mode` opens one section. Every form the site uses works, so a copied `https://caudra.ai/docs/...` link does too. Any other text, such as `/docs shell timeout`, opens a search for those words.

Press `/` to search every page. A section matches when it contains every word, ignoring case and matching inside longer words, and a misspelt word of five or more letters also matches the closest words in the docs. Sections with the words in their heading come first. `Enter` opens a result with the words highlighted, and `F3` and `Shift+F3` move between the highlights. `Esc` returns to the page and keeps the query for the next `/`.

`Tab` and `Shift+Tab` select a link on screen, and `Enter` follows it. A link to another page opens here, and a link out of the docs opens in your browser. `Backspace` or `[` goes back, and `]` goes forward again. `n` and `p` jump to the next or previous heading. A click follows a link or opens a contents row or a result. A drag over the page copies the [Markdown source](/docs/markdown/#copying) of what it covers, as in the transcript. While the pointer is on a link, the bottom row shows where it goes: the page and section for a link into the docs, or the address of any other link.

On a terminal about 110 columns wide or more, the contents stay beside the page and mark the page and section you are reading. On a narrower one, `c` shows the contents in place of the page. Typing there filters the pages, and `Tab` or `Esc` returns to the page. The agent reads the same pages through the [`caudra-docs` skill](/docs/skills/#caudra-docs)."#;

const LOGS: &str = r#"## Logs

`/logs` opens the structured log Caudra writes for every run. It reads only the rows on screen plus a small buffer, so the modal opens at the same speed on a 5 KB file and a 5 MB one.

New records arrive while the modal is open. Press `f` to pause that and read a fixed view, then `f` again to resume. Scrolling up pauses on its own, and jumping to the end resumes.

Press `l` to cycle the minimum level, or click the level in the footer. Press `Enter` to expand the selected record into every field, its spans, and the raw JSON, `y` to copy the record as shown, and `Y` to copy the stored line.

A record is drawn with every field it has, so a wide one runs past the right margin. Press `w` to wrap it onto as many rows as it needs, or use the arrow keys to pan across it and `Home` to come back. The hint row offers the arrows only while something is out there to reach, so their absence means the record already fits.

Press `Tab` to keep only the records sharing the selected one's narrowest id, which is its tool call, then its request, then its session. That id lands in the filter field, so it can be widened or cleared like anything else typed there. A record carrying no id says so rather than filtering to nothing.

Press `/` to filter. Each term matches as a subsequence, so `tolcal` finds `tool_call`, and a term is compared against the message, the target, the level, and each field and span value on its own. Space separates terms, and a record has to match all of them, so `provider retry` finds a retry from the provider. Filtering reads backward through the rotated files, and the footer says when it reached the oldest one or stopped at the scan limit.

A line the parser cannot read, such as a panic backtrace, is shown as it was written and treated as an error so a filter never hides it.

Prompt text and tool input are not written to the log unless you opt in. See [Telemetry](/docs/telemetry/#what-does-not). Run `caudra logs` for the same records outside the TUI, and see [Logging](/docs/logging/) for the file, its rotation, and the level."#;

const REQUIREMENTS: &str = r#"## Requirements

`/extract` reads every message you wrote in the session, every answer you gave to a `question` call, and the reply you were answering each time, so a `fix this too` or an `ignore docker` resolves to what it meant. A long reply is cut to its beginning and end. Bare steering such as `go`, `yes`, and `commit this` is left out, and the Extract model turns the rest into the list of requirements they add up to. It reads across compactions, so a constraint stated at the start still makes the list after the summary replaced that turn. Later messages win: a changed number, name, or approach appears once, as the current position, with a `(was: …)` note when the earlier one is worth knowing. On a session too long for the model's window, the list carries the previous extraction forward. The list streams into a modal as it is written. Press `y` or click `Copy` to hand what has arrived so far to the clipboard, and press `Esc` or click `Close` to close. The request never enters the session's history and bills under the `extract` purpose.

The same extraction runs beside every automatic or manual compaction of the main session and lands in the summary as a `# User requirements` section, so the model keeps working from your terms after the turns that stated them are gone. The summary model is told to leave that section alone and the extractor writes it fresh each time, from the whole transcript. When the extraction fails or finds nothing, the section from the previous summary is carried forward. Set `agent.compaction_requirements = false` to compact without it. Subagents summarize their own transcripts without a requirements section.

Extract is a [model job](/docs/providers/#model-jobs) and follows Fast unless bound in `/model`."#;

const GOALS: &str = r#"## Completion goals

`/goal <condition>` asks Caudra to keep working until the conversation contains evidence that the condition is met. One goal can be active per session, and a new condition replaces the current one. Conditions are limited to 4,000 characters.

At the end of each natural work turn, a separate model call evaluates the condition against a private copy of the transcript. Background tasks, shell commands, workflows, and their result delivery must settle first. The main agent processes their results before the turn is complete. Queued user input takes priority over a goal check. The evaluator has no tools and its messages do not enter the conversation. A met goal clears itself. An unmet goal adds hidden guidance and starts another work turn. A genuinely impossible goal stops with the evaluator's reason and clears itself.

Run `/goal-model` to open the Goal assignment page directly. `/goal model` is also accepted as an alias. Left unbound, Goal follows Fast. See [Model jobs](#model-jobs) for other assignments and failure behavior.

Run `/goal` without arguments to open the status panel. It shows the condition, evaluator, elapsed time, evaluation count, spend, latest reason, and automatic-continuation limit. Use Left and Right or `-` and `+` to adjust the limit for the current session. The footer shows a compact indicator while a goal is active, and clicking that indicator opens the panel.

Use `/goal-clear` to stop early. `/goal clear` remains an alias, and `stop`, `off`, `reset`, `none`, and `cancel` are also accepted after `/goal`, without regard to case.

Goal state belongs to the session. An active goal survives resume with its condition, evaluation count, spend, elapsed time, and latest reason, and completed status survives too, while `/new` clears them. A resumed goal is status rather than a trigger: it waits for your next message and is evaluated at the end of that turn. Normal permissions still apply, so unattended goals need rules or YOLO mode that already permit the required tools.

Evaluator spend is recorded under the `goal` purpose in the lifetime ledger, billed to whichever provider served the evaluator. See [Lifetime spend](/docs/token-economy/#lifetime-spend).

Caudra defers evaluation while tracked background agents are running and starts a hidden check-in after they finish. Worker compaction can still run, but evaluator calls never compact or alter history.

Sixteen automatic continuations are allowed in one query by default. The `/goal` panel accepts a session-only limit from 0 through 100. When that safety cap or the configured turn limit is reached, Caudra returns control with the goal still active and reports this-run continuations separately from total evaluations. Send another message to resume. Evaluator errors also leave the goal active. Authentication, billing, context-limit, and unavailable-model errors clear it when retrying cannot recover.

One-shot headless mode accepts the same form:

```bash
caudra --print --prompt '/goal tests pass and cargo clippy is clean'
```

Headless mode waits for tracked background agents before evaluating. An impossible condition, evaluator failure, continuation cap, or turn limit produces an error result."#;

const RESUMING: &str = r#"## Resuming after an interruption

`Esc Esc` cancels the running turn. `/continue` picks the work back up without adding a message of your own, so the model reads the history it was already working from instead of a fresh instruction.

What Caudra sends depends on where the turn stopped. A turn cancelled inside a tool call resumes from that tool result, the same point the loop would have carried on from. A reply cancelled part way through has no such point, so Caudra adds one short line asking the model to continue, drawn in the transcript as an injected message rather than as something you wrote.

`/continue` also works after a turn that ended normally, which is how you ask for more work without writing a prompt. It reports why it did nothing when the session is busy or when the session has no history yet.

Subagents resume the same way. A `task` call that passes a `task_id` may omit `prompt`, which continues that subagent from its existing messages with nothing new to act on."#;

const STASH: &str = r#"## Stash

A prompt you are not ready to send does not have to block the composer. `/stash` (`Ctrl+X s`) moves the draft out of the way, `/stash-pop` (`Ctrl+X p`) brings the newest one back, and `/stash-list` opens the full list.

The stash keeps the whole composer, so pasted text keeps its `[Pasted N lines]` pill and attached images come back with the draft. Entries are stored in the `input.stash` row of Caudra's SQLite state database, capped at 50, and shared across every session and project. That makes the stash a way to carry a prompt from one project to another. Each entry records the directory it came from, and the list shows that name next to its age.

`/stash-pop` and the list both refuse to restore into a composer that already holds a draft. Stash the current one first, then restore. In the list, `Enter` restores an entry and removes it, and `Ctrl+D` twice deletes without restoring.

Stashing is for drafts you do not want to send yet. To line up prompts Caudra should send on its own, use the queue instead. See [Queue and Steering](/docs/queue/)."#;

const TASKS: &str = r#"## Tasks

Each `task` subagent has a separate transcript. Open the task picker with `/tasks` or `Ctrl+X a`, click the task count above the input, or click a task call in the main chat. Click `[< Main]` in a task's status bar to return. The picker also lists Main and supports previewing every transcript.

`/tasks list` opens or refreshes the picker. `/tasks status <task_id>` opens it with that task selected and its details visible. Press Enter or click an agent row to open its chat. Workflow agents are listed too, and one without a transcript opens its run in the workflow inspector. Escape restores the original focus. Inspection stays local and does not send a model prompt or add transcript messages. Shell commands have their own modal, described in [Shell commands](#shell-commands). `/tasks status` with a shell ID opens that modal instead.

`/tasks background <task_id>` lets a foreground agent task continue asynchronously without restarting it when `agent.task_execution = "auto"`. `/tasks cancel <task_id>` cancels that invocation. The picker offers `Ctrl+B` to promote and `Ctrl+K` to cancel an eligible selected task. Ordinary letters filter the list. Main and finished tasks have no execution controls. See [background tasks](/docs/sessions/#background-tasks) for execution policies, delivery, and shutdown behavior.

Background rows show a right-aligned `bg` indicator beside the state or spinner, including after completion. New task IDs come from the description, such as `implement-active-footer-chips`, with `-2`, `-3`, and later suffixes for collisions. Display labels stay unchanged. Shell jobs use safe command labels such as `shell-cargo-test`, falling back to `shell`. Use the returned ID unchanged. Older IDs remain valid for inspection, control, and continuation. See [task and output IDs](/docs/sessions/#task-and-output-ids) for naming and compatibility.

An input box appears while the focused task is running. Press Enter to queue guidance for its next turn boundary. Pending guidance stays visible above the input until the subagent consumes it. Task transcripts survive session reloads, and later `task` calls can continue one by passing its `task_id`.

That input box is a full composer. Typing `/` opens the palette, `Ctrl+S` inserts a file path, `Ctrl+X e` edits the draft in the [workbench](/docs/workbench/#plans-memory-notes-and-prompt-drafts), and `Ctrl+V` attaches an image to the guidance. A custom `/project:` or `/user:` command expands its template and steers the focused task rather than the main session.

Commands that reach the main session's turn or history have no task equivalent, so `/compact`, `/continue`, `/model`, `/system-prompt`, `/projection`, `/btw`, `/extract`, the `/goal` family, the workflow commands, and MCP prompts are drawn dimmed and report their scope when run. Return to Main to use them. `/context`, `/tools`, `/skills`, `/queue`, `/review`, and the stash commands already follow the focused transcript."#;

const SHELLS: &str = r#"## Shell commands

`/shells` opens the shell modal. It lists the `shell` calls that the main agent, subagents, and workflow agents made in this session, in the foreground or the background. Running commands come first, oldest first, and finished ones follow, newest first. Each row shows the first line of its command, highlighted as shell code. Background rows show the same `bg` indicator as tasks. Click the shell count above the input to open the modal on the oldest running command. Commands you run with `!` or `!!` are not listed.

Press Enter or click a row to see the whole command with its owner, working directory, timeout, elapsed time, exit status, and output. A command longer than 8 KiB is shown without highlighting. Output appears as the command wrote it, with terminal control sequences escaped and no Markdown rendering. Output of a running command updates in place. Ordinary letters filter the list. Escape leaves the details, and a second Escape closes the modal without stopping anything.

`Ctrl+K` stops the selected running command, from the list or its details. Only that call is cancelled. The row shows `cancelling` until the command settles. Its owner receives an ordinary cancelled result, and the rest of its work continues. Background shell jobs also accept `/tasks cancel <task_id>`. Shell jobs cannot be promoted or resumed.

Finished commands stay listed after a reload. See [shell history](/docs/sessions/#shell-history) for what a session keeps and how commands cut short by a crash appear."#;

const WORKFLOWS: &str = r#"## Workflows

Workflows are experimental. The commands here exist only with `workflows = true` under [`[experimental]`](/docs/configuration/#experimental-features).

A workflow is a script that launches subagents in phases, keeps a journal, and can be paused and resumed. Each session runs one workflow runtime. Runs continue after a normal main-turn completion. `Esc Esc` stops the main turn, background tasks, and workflows, and suppresses automatic completion turns. A run belongs to the session that started it and stays with that session when you switch to another.

New workflow run IDs use three words from the plan-name word lists, such as `neat-wanted-cowbird`. A collision adds a numeric suffix. Existing run IDs remain valid, and resuming a run keeps its original ID.

`/workflows` opens the catalog: every script from the built-ins, the project's `.caudra/workflows/`, and your user config, with the ones that failed to parse listed under it. A project or user script runs only after its content digest has been trusted. `Enter` on an untrusted entry shows the digest and asks you to confirm it, and a script that changes on disk needs trusting again. `Enter` on a trusted entry fills the composer with `/workflow <name> ` so you can add the arguments.

`/workflow <name> [--agent-budget N] [args]` starts a run. Arguments written as a JSON object are handed to the script as they are. Any other text becomes its `query` and `objective`. `--agent-budget` caps how many agents the run may admit. Starting an untrusted script reports it and points you at `/workflows`. `/deep-research <query>` is `/workflow deep-research <query>`.

`/workflow` alone, or `/workflow runs`, opens the runs picker. Each run lists its phase, agents admitted, tokens used, and the agents it launched, with their state. Press `p` to pause the selected run, `r` to resume it, and `x` to stop it. `Enter` on an agent row opens that agent's transcript, the same view the [task picker](#tasks) gives. The same controls take a name from the command line: `/workflow pause <run>`, `/workflow resume <run>`, and `/workflow stop <run>`, where `<run>` is the display name shown in the picker or the run id.

The status bar shows `[wf:N]` while N runs are working, and `[wf:N+M]` once M runs are paused or out of budget and waiting on you.

A run that finishes, fails, pauses, or runs out of budget leaves a notice. When the main chat is ready, Caudra starts a turn carrying pending notices so the model can act on them. Other background work can continue. The notice names the run and its status, then the `report` string of its result or the whole result when there is none, the scratch file path when the script wrote one, and the pause message or error. Reports are cut at 8 KiB. A run revision is acknowledged after its observation is durably saved in the main history. The [headless surface](/docs/headless/#completion-context) uses the same receipt rule.

Closing Caudra interrupts every active run, and an interrupted run is over. Pause a run you mean to pick up later: resuming a paused run replays its journal and continues from the last committed phase, and work an agent had started but not committed runs again. The model reaches the same runtime through the [`workflow` tool](/docs/tools/#workflow)."#;

fn write_row(out: &mut String, command: &BuiltinCommand) {
    let scope = match command.scope {
        ChatScope::Any => "",
        ChatScope::MainOnly => MAIN_ONLY_MARK,
    };
    let experiments = command
        .features
        .iter()
        .map(|feature| format!("`{}`", feature.key()))
        .collect::<Vec<_>>()
        .join(" or ");
    writeln!(
        out,
        "| `{name}` | {description} | {scope} | {experiments} |",
        name = command.name,
        description = command.description.replace('|', "\\|"),
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
    writeln!(
        out,
        "A command with an Experiment entry exists only while that switch is on under `[experimental]` in the global `caudra.toml`. Typing it while the switch is off names the switch instead. See [Experimental features](/docs/configuration/#experimental-features)."
    )
    .unwrap();
    writeln!(out).unwrap();
    writeln!(out, "| Command | Description | Scope | Experiment |").unwrap();
    writeln!(out, "|---------|-------------|-------|------------|").unwrap();
    for cmd in BUILTIN_COMMANDS {
        write_row(&mut out, cmd);
    }

    writeln!(out).unwrap();
    writeln!(out, "{RESUMING}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{MODEL_JOBS}").unwrap();

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
    writeln!(out, "{SHELLS}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{WORKFLOWS}").unwrap();

    writeln!(out).unwrap();
    writeln!(out, "{GOALS}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "{CONTEXT}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "{DOCS}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "{LOGS}").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "{REQUIREMENTS}").unwrap();
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
        "- **`/thinking`**: extended thinking. Optional arg: `off`, `adaptive`, an effort level (`minimal` … `max`), or a token budget number. The level is remembered across restarts. Config: `always_thinking` overrides the remembered level. The status bar names the level it resolved to, shortened to its first two letters on a narrow terminal (`[xh]` for `xhigh`). A token budget keeps every digit. A task footer names the level that task runs at, which its own model resolved and a system prompt profile may have overridden. The setting behind the chip belongs to the session, so on a task the chip is a label rather than a control."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/fast`**: fast mode, a premium tier that answers quicker for more per token. Anthropic Opus sells one, and so does every GPT-6 and GPT-5.6 model. Ignored on a model without one, and unavailable on a subscription. Config: `always_fast = true`."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/view`**: cycle the transcript through expanded, auto, and compact. Expanded is the default when no saved choice exists and gives each call its own card. Saved choices are respected across restarts. In auto, every card falls back to a single row except the newest one, which stays open until a newer card replaces it. In auto and expanded only calls that changed nothing can be hidden, so writes and edits stay open, though you can still click one shut. Compact is the one mode that answers for every tool: every call folds to a single row, writes and edits included, and a click opens the one you want. `batch` is the single exception there, because its body is the list of the calls it made and folded it would say nothing at all. Its children fold instead, each to its own row. Tools named in `ui.always_collapsed` stay a single row in every mode until you click them, which suits the lookups whose first line is already the answer. A server-qualified name matches too, so `file_read` also covers `mcp_File_read`. An open card shows as much of its body as `ui.tool_output_lines` allows for that tool, clicking shows all of it, and clicking again puts it back. A card you opened yourself stays open as the transcript grows. `shell`, `python_execution`, and `task` are drawn differently: their body is a fixed window of `ui.scroll_card_lines` rows that follows the newest output, with a footer reporting how much sits above and below. Click inside a window to give it the wheel, or drag the bar in its last column. While the call is still running the footer names the edge it is pinned to, scrolling up pauses it and says so, and clicking that footer sends it back to the tail. Once the call has answered there is no tail left to follow, so the footer reports only how much sits either side. A write is never windowed, and a write that created a file is never abridged either, since its body is that file. Anything drawn as a diff — an edit, a patch, an overwrite — is drawn whole until it runs long, because a diff is already only the part that changed, so `ui.tool_output_lines` bounds one only where it is raised past that point. Outside compact, `task` and `batch` keep their child rows throughout, and each child answers the same question its own card would: a child that changed something draws its body the way its own card would draw it, and every other child folds to its row until you click it. Nesting goes one body deep. A batch inside a subagent keeps a row per child, with its status and its clock, but folds its bodies away until you open one, so work arriving at the bottom of a tree cannot keep resizing everything above it."
    )
    .unwrap();
    writeln!(
        out,
        "- **Plan / build**: not a slash command. Press `Tab` in the input to toggle plan mode (plan-file writes only). Caudra opens in plan mode, and a resumed session reopens in the mode it was left in. A toggle reaches the agent with your next message, so until you send one the status bar shows the pending switch as `[PLAN\u{2192}BUILD]`. It abbreviates this to `[P\u{2192}B]` when those columns preserve more useful footer detail. Each mode remembers the model it was last used with, so the toggle asks for that one too and the bar names the pair as `[claude-opus-5\u{2192}claude-sonnet-5]`, keeping the provider only when the two differ there. Binding the Plan job in `/model` decides what a plan run uses on its own, and turns the swap off."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/reload`**: read `caudra.toml` again and rebuild plugins without leaving the app. The `[experimental]` switches apply from startup, so changing them needs a restart."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/btw`**: a side question over the conversation so far, with follow-ups in the same thread. The answer streams into a modal that reports elapsed time while it waits, and how much of the prompt has been prefilled when the provider says. Type under it and press `Enter` or click `Send a follow-up` to ask the next question, which is queued if the previous answer is still streaming. The follow-up field edits like the composer, with the keys under [Text Fields](/docs/keybindings/#text-fields). `Ctrl+Y` copies the answer. `Ctrl+C` copies the text selected in the field, and with nothing selected it stops the answer without losing the thread. `Esc` or `Close` closes. No tool runs, nothing enters history, and a marker in the transcript shows where the thread's view of the conversation ends."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/extract`**: list every requirement the session has gathered so far. See [Requirements](#requirements)."
    )
    .unwrap();
    writeln!(
        out,
        "- **`/memory`**: open the memory file picker. `Enter` opens a note in the [workbench](/docs/workbench/#plans-memory-notes-and-prompt-drafts) to read or edit, and a note can be deleted from the list. See the `memory` tool under [Tools](/docs/tools/)."
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
