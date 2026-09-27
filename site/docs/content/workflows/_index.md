+++
title = "Workflows"
weight = 24
[extra]
group = "Guides"
+++

# Workflows

A workflow is a script that runs a plan of subagents and keeps the results. It is the trusted part of the system: plain code that fans work out to agents, validates what comes back, drops what does not hold up, and assembles the result. The agents are untrusted workers. The script decides.

Caudra ships three workflows and discovers the ones you write. Runs are durable. A run that pauses, fails, or is stopped can resume from its journal instead of starting over.

## What ships

| Workflow | Use it for | Phases |
|----------|------------|--------|
| `deep-research` | A question that needs sourced claims rather than an answer from memory | Plan, Research, Verify, Report |
| `review-changes` | A diff, branch, or pull request you want read by more than one pair of eyes | Survey, Review, Refute, Report |
| `root-cause` | A failure whose cause is not obvious from the error alone | Evidence, Hypothesize, Refute, Report |

All three are read-only. They inspect the workspace and the web, and they write their report to a scratch file. None of them edits your code.

They share a shape. Work fans out to agents that cannot see each other, a second wave attacks what the first wave produced, and the script throws away whatever does not survive. `deep-research` verifies claims against independent sources. `review-changes` hands each finding to a refuter that tries to show the defect is not there. `root-cause` proposes rival causes from separate stances and rules out the ones the code contradicts. What reaches the report has been through an adversary.

Each one degrades rather than failing. A reviewer that returns nothing costs you that angle and a line in the coverage section. A run with too little budget left to refute says so in the report and labels its findings unchallenged.

## Run one

```
/deep-research Compare the migration risks of PostgreSQL 17 and MySQL 9
/workflow review-changes the auth middleware I just rewrote
/workflow root-cause the integration suite panics on startup since Tuesday
```

The command returns at once. The run continues in the background while you keep working in the session. Watch it in `/workflow`, and when it settles Caudra starts one model turn that carries the report into the conversation.

Other ways to launch:

```
/workflow deep-research --agent-budget 12 how does smol schedule blocking work
/workflow deep-research {"query": "smol blocking", "breadth": 3}
```

Plain text after the name becomes `args.query` and `args.objective`. A JSON object passes through as `args`. `--agent-budget N` caps how many agents the run may launch. `review-changes` also reads `args.scope` and `root-cause` reads `args.failure`, so a JSON launch can name the input directly.

The model can launch workflows too, through the `workflow` tool, when the session has a runtime. It sees the same catalog you do and the same trust rules. See [Tools](/docs/tools/#workflow) for the tool contract.

## Watching a run

A launch draws a card in the transcript, whether you typed the command or the model called the `workflow` tool. The header names the run and reads its status, phase, agents admitted against the budget, and tokens spent. Below it sits the phase strip, the agents working right now, and the last three log lines. When the run settles the log lines give way to the report, the scratch file path, and any error. The report is markdown a model wrote, and the card draws it the way the transcript draws every other model answer. A run that spent its budget says so on the card and asks for a higher one. Click the scratch file path to open it in the [workbench](/docs/workbench/). Click anywhere else on the card to open the inspector on that run. A card the tool drew is part of the tool result and comes back on restore, brought up to date from the runtime, because the stored copy is frozen at the moment of launch and the run is not. The card of a slash launch is not saved with the session, so a resumed transcript keeps a one line notice of what the run came to instead.

The status bar keeps a chip while any run is going. One active run shows as `[wf: deep-research · Research 2/4]`, with its phase and where that phase sits among the ones the script declared. Several runs, or runs parked waiting on someone, show as `[wf:2+1 · Research]` with the newest run's phase. A narrow bar drops the phase before it drops the chip. Click the chip to open the inspector.

Workflow agents are ordinary subagents. They ask for permission through the normal prompts, respect the current permission mode as a ceiling, share `task_max_concurrent` with `task` calls, and open in the same transcript viewer. They do not appear as task cards in the main transcript. A prompt raised by a workflow agent names the run, the phase, and the agent label, because a run outlives the turn that started it and the task id alone identifies nothing you can see.

`Esc Esc` stops the main turn and all session tasks and workflows and suppresses automatic continuation. Stop one run from the inspector when other work should continue. See [Stop and replace](/docs/queue/#stop-and-replace).

## The inspector

`/workflow` (or `/workflow runs`, or the leader key followed by `k`) opens the inspector. Clicking a run card or the status bar chip opens it on that run. The left pane lists runs grouped as running, waiting, finished, and earlier sessions. Each row names the run, then its phase or the session that ran it, and holds its clock at the right edge. Type `/` to filter by name or session title. The pointer marks whatever it rests on: a run row or a section tab marks itself without acting, and a row inside a section takes the cursor, so a single click opens it. The right pane has four sections, reached with Tab, Shift+Tab, or the digits `1` to `4`:

| Section | Contents |
|---------|----------|
| Overview | One line in the card's shape carrying status, phase and where it sits among the declared ones, agents landed against the roster, agents admitted against the budget, tokens, and elapsed time, then the objective, the phase strip, and the last log lines |
| Timeline | Everything the run did, in the order it did it |
| Agents | The roster gathered under the phase that dispatched each agent, as a ledger of who ran and what they cost |
| Result | The report, drawn as the markdown it is, or a result with no report as a JSON tree, then the scratch file path and the pause message or error. Enter, or a click on the path, opens the scratch file in the workbench |

### Narrow terminals

Two panes split out of too few columns are two panes too narrow to read, so below the width both need the inspector shows one at a time and Left and Right move between the run list and the detail. A tab strip without room for the section names shows the digits that select them. A footer without room to gloss its keys shows the keys alone, because a footer wider than its row answers no clicks at all.

### The timeline

The timeline is the record. It puts the phases a run entered, the `agent`, `parallel`, and `write_scratch_file` calls it made, the lines it logged, and how it settled onto one clock, so reading what happened does not mean matching timestamps across several lists by eye.

Calls and log lines are indented under the phase that was open when they happened. A call belongs to the phase that held the clock at the moment it started, so a phase the run entered twice counts each visit separately. Phases the script declares and the run never reached trail the walked ones, dimmed. The final row is how the run ended.

Every row reads in the same columns: when it happened on the run's clock, then what it is called, then how long it took, then its bar. A label too long for its column is cut instead of pushing the columns along, so the clocks and the durations stack down the section rather than wandering with the length of the names beside them.

Every phase and every call carries a bar scaled to the whole run. A phase that took most of the run looks like it, and a fan-out whose agents ran at the same time shows overlapping bars while one that serialised shows a staircase. A pane too narrow for a useful bar leaves it out, along with the counts that follow it, and spends the columns on the names instead.

Enter opens the row under the cursor. On a phase it moves to the first agent that phase dispatched. On a `write_scratch_file` call it opens the file. On an agent call it opens what that agent was asked and what it answered, fetched in full from the journal rather than cut to a preview.

A prompt or a result that is JSON opens as a tree rather than as a dump. Every object and every array is a row of its own and Enter on one closes it and says how many keys or items it took with it. Everything starts open, so a body that is small reads without a keystroke and one that is large can be cut down to the part being read. Each part of a call folds on its own, so closing a node in the prompt leaves the result as it was. The nodes take their place in the same cursor the rest of the section uses, so the arrows walk into a body and out the other side.

Keys carry a colour of their own. A JSON grammar scopes a key and a string value alike, which leaves every theme painting both the same, so the key is painted rather than parsed. Strings, numbers and literals keep the colours the syntax theme gives them.

### Agent rows

An Agents row reports the phase that dispatched it, its state, its tokens, and its duration. A running agent also reports what it is doing and the tools it has called. An agent that stopped keeps the last thing it was doing, dimmed. Enter opens the same request and result a timeline row opens, because the roster and the journal are two views of one call.

| Key | Action |
|-----|--------|
| `p` | Pause the selected active run |
| `r` | Resume a paused, failed, or cancelled run, or ask for a higher budget when the run spent the one it had |
| `s` | Stop the run |
| Left / Right | Move focus between the run list and the section |
| Up / Down | Walk the list, the section rows, or scroll the section text |
| Enter | Open the row under the cursor |
| `t` | Open the transcript of the agent under the cursor, from the timeline or the roster |
| `o` | Open the script the run executed |
| `e` | Copy the whole run as markdown, every prompt and result included |
| `y` | Copy the visible section as text |
| Esc | Close |

A control that does not apply to the selected run stays where it is and says why when you press it. A builtin workflow is compiled into Caudra, so `o` reports that there is no file to open.

A transcript is a chat rather than an overlay, so opening one closes the inspector. Reopening it returns to the run you left.

Runs of earlier sessions can be read but not controlled. Resume one from the session that launched it.

The same controls exist as text: `/workflow pause <name>`, `/workflow resume <name> [budget]`, `/workflow stop <name>`. A name can be a prefix of the run name or of its id, and a prefix that several runs answer to lists them instead of picking one. Launch the same workflow twice and the second run is `deep-research-2`.

## Where definitions live

`/workflows` opens the catalog. Each entry shows its source, description, phases, path, and whether it is trusted. Invalid files stay visible with the reason, so a typo in a script is found in the catalog rather than at launch.

| Scope | Location | Trust |
|-------|----------|-------|
| Built-in | compiled into Caudra | always |
| Project | `<project root>/.caudra/workflows/*.rhai` | per exact source digest |
| User | `~/.config/caudra/workflows/*.rhai` | always |

The user directory is created the first time a workflow-capable session starts, so it is ready to drop scripts into. Debug builds use `~/.config/caudra-debug/workflows/` instead, following the platform directory rule in [Configuration](/docs/configuration/#directory-layout).

The first scope that defines a name wins. A built-in name cannot be shadowed. Within one scope, two files that declare the same `meta.name` are both invalid.

A project script runs only after you approve it in `/workflows`. Approval is keyed to the SHA-256 digest of the exact bytes, together with the workflow language and host ABI versions. Change one byte and the script is untrusted again until you review it. Neither the model nor a script can grant trust.

Caudra reads each file once and uses that buffer for the digest, the catalog, compilation, and execution. A file replaced between the check and the launch cannot swap in different code.

## Write one

You can ask the agent to write one. The builtin `caudra-workflow-dev` [skill](/docs/skills/#caudra-workflow-dev) carries this whole reference plus worked examples and the Rhai details that trip people up, and the agent validates the result with the `workflow` tool before handing it over. The `list` action tells it which directory to use.

Workflows are [Rhai](https://rhai.rs) scripts. The file name must be `<meta.name>.rhai` and the first statement must be the metadata:

```rhai
let meta = #{
    name: "release-notes",
    description: "Turn the commits since a tag into notes grouped by audience",
    when_to_use: "Write release notes or a changelog for a range of commits.",
    phases: [
        #{ title: "Read", detail: "Independent readers summarize the commits" },
        #{ title: "Report", detail: "Merge the summaries into one set of notes" },
    ],
};
```

`name` is kebab-case, at most 64 bytes. `description` is required. `when_to_use` is what the model reads when deciding whether to launch the workflow, so write it for the model. `phases` drive the progress display and are optional.

Everything after the metadata is ordinary Rhai with these host functions:

| Call | Result |
|------|--------|
| `agent(prompt)`, `agent(prompt, opts)` | One subagent, blocking. Returns `#{ agent_id, success, output, cancelled, tokens_used, duration_ms }` |
| `parallel([opts, ...])` | Runs every item concurrently and returns their results in input order |
| `phase(title)` | Moves the progress display to that phase |
| `log(message)` | Appends to the run log |
| `pause(kind, message)` | Suspends the run in a resumable state |
| `complete()`, `complete(value)` | Ends the run with `value` as its result |
| `write_scratch_file(name, content)` | Writes an artifact and returns its path |
| `json_encode(value)` | Serializes a value to JSON text |
| `budget()` | Returns `#{ issued, limit, remaining }` for the run's agent budget |
| `args` | The launch arguments, or `()` when none were given |

Agent options:

| Option | Meaning |
|--------|---------|
| `prompt` | The instruction. Required in `parallel` items, and the positional argument to `agent` |
| `label` | The name this agent takes in the roster and the timeline |
| `capability_mode` | `read-only` for a plan task, or `read-write`, `execute`, `all` for a build task |
| `output_schema` | A JSON Schema the agent must satisfy through `structured_output` |
| `phase` | The phase the call belongs to |
| `profile` | A Caudra task profile, which brings its own system prompt and tool set |
| `model_job` | The kind of work this call is: `chat`, `plan`, `subagent`, `fast`, or `best` |

An unknown option is an error. A build agent is still capped by your current permission mode.

A `read-only` agent reads, searches, and fetches, and it may also run a shell command when the command changes nothing and every path it touches stays inside the project. `git diff` and `rg` work. A build or an edit is refused per call, with a message saying which call and why. See [read-only agents](/docs/permissions/#read-only-agents).

### Choosing a model

`model_job` names a job rather than a model. The job resolves against the model bindings configured on the machine the workflow runs on, so a script shared between two people picks each person's fast model rather than pinning one they may not have. Naming a model directly, as `anthropic/claude-haiku-4-5`, is rejected.

Use `fast` for bulk work over a fixed packet, such as a shard of findings to rule on. Use `best` for the one call whose output the user reads. Leave it off, or pass `subagent`, for everything else, and the call runs on whatever the session uses for subagents.

Precedence runs from most specific configuration to least. A `subagent_model` pin on the profile the call names wins over the call's `model_job`, because a user who pinned a model for a profile meant it. Without a pin, `model_job` applies. Without either, the session's subagent binding applies. See [System Prompt Profiles](/docs/system-prompts/#configure-subagents).

### Spending the budget

`budget()` reports what the run has spent. `issued` counts every agent the script asked for, including each item of a `parallel`, `limit` is the budget the run was launched with, and `remaining` is the difference.

Read it to decide how much verification you can still afford:

```rhai
if budget().remaining > shard_count {
    let verdicts = parallel(refute_jobs);
    // drop what the refuters ruled out
} else {
    notes.push("The budget ran out before refutation, so these findings are unchallenged.");
}
```

The count comes from what the script asked for rather than from what the host admitted, so a branch on `budget()` reads the same on a resume as it did on the original run. Both built-in review workflows use it to skip refutation and say so in the report rather than running out of budget in the middle of one.

`output` is validated JSON when `output_schema` was given, otherwise the agent's final text. Treat both as data. The engine converts model output to inert values, so nothing an agent returns can become a host call or a permission.

A host failure such as an agent that errors is a catchable Rhai error, which is why `deep-research` wraps its planner in `try`. Cancellation, budget exhaustion, `pause`, and `complete` end the run from outside the script and cannot be caught.

Nothing else is reachable. There is no `import`, `eval`, clock, sleep, or file access. A script sees the host functions above and Rhai's standard string, array, and map functions.

Validate a script from the catalog or with the `workflow` tool's `validate` action. Validation compiles the script and runs it once against a canned host whose agents return empty results, so it exercises one path through the code, not every branch.

## Patterns that work

The three built-in workflows are worth reading as worked examples. These are the patterns they share.

### Fan out, then attack

One agent is a single opinion. Two agents asked the same question are two correlated opinions. The useful shape is a wide first pass followed by a second pass whose only job is to destroy what the first pass produced.

Give the second pass a packet and a narrow ruling. `review-changes` asks its refuters to rule `upheld`, `refuted`, or `uncertain`, and forbids them to repair a finding or add one. A refuter allowed to improve a claim will improve it, which is how a weak finding survives.

Drop what loses. Keeping a refuted item with a warning label puts the judgement back on the reader.

### Make the angles disjoint

Independence comes from the topology rather than from asking for it in the prompt. Agents that can see each other's work converge, so each one gets its own slice and no sight of the others.

`review-changes` splits by dimension: correctness, security, performance, tests, API compatibility. `root-cause` splits its evidence pass by strand and its hypothesis pass by stance, which is the same idea applied to explanation rather than observation. Each prompt names its slice and says another agent covers the rest.

### Shard the verify pass

A verifier handed twenty items will skim. Shard the work so each verifier sees a handful, and check the result as a bijection: exactly one verdict per ID, every ID accounted for, no ID that was not in the packet.

Fail the shard rather than the item. A verifier that returned a malformed set has told you nothing about any of its items, so discard all of them and say so in the report.

### Degrade on purpose

Decide in advance what a run does when it runs short, then say what it did.

```rhai
let spend = budget();
if spend.remaining <= shard_count {
    degraded = true;
    notes.push("Only " + spend.remaining.to_string() + " agent(s) remained, so nothing was refuted.");
} else {
    // run the refuters
}
```

A report that says which pass it skipped is worth more than one that quietly skipped it. Both review workflows carry a coverage section listing every angle that failed and every item dropped.

### Frame every interpolation

Everything an agent returns is untrusted. Wrap it before it reaches another prompt:

```rhai
fn untrusted(tag, value) {
    "<" + tag + "-json>\n" + json_encode(value) + "\n</" + tag + "-json>"
}
```

`json_encode` makes the content inert text, and the tag tells the reading agent where the data starts and stops. Say in the prompt that the block is data rather than instructions.

### Withhold what you do not want invented

An agent that is shown a conclusion will support it. Refuters in `root-cause` receive the causes and the observations, and nothing about which cause the script currently favours. The ranking happens after the verdicts arrive, in the script.

### Two profiles, two budgets

Pair a cheap wide pass with one expensive narrow one. Set `model_job: "fast"` on the sharded verifiers, where the work is mechanical and the packet is fixed, and `model_job: "best"` on the single call that writes what the user reads. A run of twenty agents where nineteen are cheap costs about what one careful agent costs.

## How resume works

Every `agent`, `parallel`, and `write_scratch_file` call gets a sequence key and a hash of its request. Results are committed to the session database before the script sees them. Resuming a run evaluates the same source from the top with the same `args`, and each call whose key is in the journal returns the committed result without touching an agent. The script reaches the point where it stopped and continues from there.

Divergence is an error. If the script asks a different question at the same key, the run fails rather than replaying a stale answer.

Resume is not exactly-once for the outside world. An agent that edited files before a pause landed, whose result was not yet committed, runs again on resume and may repeat that work. The three built-in workflows are read-only and unaffected. A build workflow should make its agents idempotent or keep them small.

A run that was active when the process exited becomes `interrupted` and does not resume. External effects have no stable identity across processes, so replaying them would be a guess. Start a new run.

Run statuses: `active`, `paused`, `budget_limited`, `interrupted`, `completed`, `cancelled`, `failed`. A `budget_limited` run resumes only with an `agent_budget` above the count already admitted. Pressing `r` in the inspector asks for one and offers a step up from what the run spent, and `/workflow resume <name> <budget>` carries it from the command line.

## Artifacts and storage

`write_scratch_file` writes below the state directory under `workflow_scratch/<session>/<run>/`. Names are single path components. Each run may write 64 files, 1 MiB each, 16 MiB in total. Scratch files count toward the session's storage and are removed with it.

Runs and their journals live in the session database and are deleted with the session. Each run also keeps a timeline of its phase changes and `log` lines, up to 512 rows, the oldest log lines going first. The inspector reads it, and so does the `workflow` tool's `inspect` action. Trimming a session drops the journals and timelines and marks unfinished runs `interrupted`. A fork does not copy running workflows.

Runs of earlier sessions in the same state directory stay readable. The inspector lists the twenty most recent under "Earlier sessions", and the `workflow` tool's `history` action returns up to fifty with the session that ran each one.

## Limits

| Limit | Value |
|-------|-------|
| Active runs per session | 4 |
| Agent budget | 128 by default, 256 at most |
| Source size | 256 KiB |
| Launch arguments | 1 MiB |
| Result size | 4 MiB |
| Host calls per run | 10 000 |
| Wall time per run | 4 hours |
| Script operations | 50 million |
| Journal | 16 384 entries, 64 MiB |

Every workflow agent also takes a slot from `task_max_concurrent`, so a `parallel` of six with a limit of eight leaves two slots for everything else while they run.

## Other frontends

The stream-JSON SDK exposes the same catalog and controls over `control_request` and reports runs as `system` messages. Like the TUI, it starts an automatic parent run for pending completion notices at a safe boundary, including after a normal final answer. See [Headless Mode](/docs/headless/#workflows) for wire events and shutdown behavior.

One-shot `--print` and ACP sessions have no workflow runtime. The `workflow` tool is not offered there.
