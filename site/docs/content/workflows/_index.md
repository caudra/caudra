+++
title = "Workflows"
weight = 24
[extra]
group = "Guides"
+++

# Workflows

A workflow is a script that runs a plan of subagents and keeps the results. It is the trusted part of the system: plain code that fans work out to agents, validates what comes back, drops what does not hold up, and assembles the result. The agents are untrusted workers. The script decides.

Caudra ships one workflow, `deep-research`, and discovers the ones you write. Runs are durable. A run that pauses, fails, or is stopped can resume from its journal instead of starting over.

## Run one

```
/deep-research Compare the migration risks of PostgreSQL 17 and MySQL 9
```

The command returns at once. The run continues in the background while you keep working in the session. Watch it in `/workflow`, and when it settles Caudra starts one model turn that carries the report into the conversation.

Other ways to launch:

```
/workflow deep-research --agent-budget 12 how does smol schedule blocking work
/workflow deep-research {"query": "smol blocking", "breadth": 3}
```

Plain text after the name becomes `args.query` and `args.objective`. A JSON object passes through as `args`. `--agent-budget N` caps how many agents the run may launch.

The model can launch workflows too, through the `workflow` tool, when the session has a runtime. It sees the same catalog you do and the same trust rules. See [Tools](/docs/tools/#workflow) for the tool contract.

## Watching a run

A launch draws a card in the transcript, whether you typed the command or the model called the `workflow` tool. The header names the run and reads its status, phase, agents admitted against the budget, and tokens spent. Below it sits the phase strip, the agents working right now, and the last three log lines. When the run settles the log lines give way to the report, the scratch file path, and any error. Click the scratch file path to open it in the [workbench](/docs/workbench/). Click anywhere else on the card to open the inspector on that run. The card of a slash launch is not saved with the session. A card the tool drew is part of the tool result and comes back on restore, brought up to date from the runtime.

The status bar keeps a chip while any run is going. One active run shows as `[wf: deep-research · Research 2/4]`, with its phase and where that phase sits among the ones the script declared. Several runs, or runs parked waiting on someone, show as `[wf:2+1 · Research]` with the newest run's phase. A narrow bar drops the phase before it drops the chip. Click the chip to open the inspector.

Workflow agents are ordinary subagents. They ask for permission through the normal prompts, respect the current permission mode as a ceiling, share `task_max_concurrent` with `task` calls, and open in the same transcript viewer. They do not appear as task cards in the main transcript.

Pressing Esc cancels the main turn and leaves workflow runs alone. Stop them from the inspector.

## The inspector

`/workflow` (or `/workflow runs`, or the leader key followed by `k`) opens the inspector. Clicking a run card or the status bar chip opens it on that run. The left pane lists runs grouped as running, waiting, finished, and earlier sessions. Type `/` to filter by name or session title. The right pane has six sections, reached with Tab, Shift+Tab, or the digits `1` to `6`:

| Section | Contents |
|---------|----------|
| Overview | Status, phase, elapsed time, agents, tokens, objective, the phase strip, and the last log lines |
| Phases | Every phase the run entered with its start offset and duration |
| Agents | The roster with state, phase, tokens, and duration. Enter opens the agent's transcript |
| Calls | The journal: each `agent`, `parallel`, and `write_scratch_file` call with its state and timing. Enter expands a call's result preview or error, or opens the file a `write_scratch_file` call wrote |
| Logs | The stored timeline of phase changes and `log` lines, following the tail |
| Result | The report or result JSON, the scratch file path, and the pause message or error. Enter, or a click on the path, opens the scratch file in the workbench |

| Key | Action |
|-----|--------|
| `p` | Pause the selected active run |
| `r` | Resume a paused, failed, or cancelled run |
| `s` | Stop the run |
| Left / Right | Move focus between the run list and the section |
| Up / Down | Walk the list, the section rows, or scroll the section text |
| `y` | Copy the visible section as text |
| Esc | Close |

Runs of earlier sessions can be read but not controlled. Resume one from the session that launched it.

The same controls exist as text: `/workflow pause <name>`, `/workflow resume <name>`, `/workflow stop <name>`. Launch the same workflow twice and the second run is `deep-research-2`.

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
    name: "review-changes",
    description: "Review a diff with independent readers and merge their findings",
    when_to_use: "Review, audit, or second-opinion a change before it lands.",
    phases: [
        #{ title: "Read", detail: "Independent readers summarize the diff" },
        #{ title: "Report", detail: "Merge findings into one review" },
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
| `args` | The launch arguments, or `()` when none were given |

Agent options: `prompt` (required in `parallel` items), `label` for the roster, `capability_mode` (`read-only`, or `read-write`, `execute`, `all` for a build agent), `output_schema` (a JSON Schema the agent must satisfy through `structured_output`), `phase`, and `profile` (a Caudra task profile). An unknown option is an error. A read-only agent runs as a plan task. A build agent runs as a build task, still capped by your current permission mode.

`output` is validated JSON when `output_schema` was given, otherwise the agent's final text. Treat both as data. The engine converts model output to inert values, so nothing an agent returns can become a host call or a permission.

A host failure such as an agent that errors is a catchable Rhai error, which is why `deep-research` wraps its planner in `try`. Cancellation, budget exhaustion, `pause`, and `complete` end the run from outside the script and cannot be caught.

Nothing else is reachable. There is no `import`, `eval`, clock, sleep, or file access. A script sees the host functions above and Rhai's standard string, array, and map functions.

Validate a script from the catalog or with the `workflow` tool's `validate` action. Validation compiles the script and runs it once against a canned host whose agents return empty results, so it exercises one path through the code, not every branch.

## How resume works

Every `agent`, `parallel`, and `write_scratch_file` call gets a sequence key and a hash of its request. Results are committed to the session database before the script sees them. Resuming a run evaluates the same source from the top with the same `args`, and each call whose key is in the journal returns the committed result without touching an agent. The script reaches the point where it stopped and continues from there.

Divergence is an error. If the script asks a different question at the same key, the run fails rather than replaying a stale answer.

Resume is not exactly-once for the outside world. An agent that edited files before a pause landed, whose result was not yet committed, runs again on resume and may repeat that work. A read-only workflow such as `deep-research` is unaffected. A build workflow should make its agents idempotent or keep them small.

A run that was active when the process exited becomes `interrupted` and does not resume. External effects have no stable identity across processes, so replaying them would be a guess. Start a new run.

Run statuses: `active`, `paused`, `budget_limited`, `interrupted`, `completed`, `cancelled`, `failed`. A `budget_limited` run resumes only with an `agent_budget` above the count already admitted.

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

The stream-JSON SDK exposes the same catalog and controls over `control_request` and reports runs as `system` messages. It does not start a turn when a run finishes. The result rides on the next prompt instead. See [Headless Mode](/docs/headless/#workflows).

One-shot `--print` and ACP sessions have no workflow runtime. The `workflow` tool is not offered there.
