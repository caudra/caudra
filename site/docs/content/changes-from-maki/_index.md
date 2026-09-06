+++
title = "Changes from Maki"
weight = 32
[extra]
group = "Concepts"
+++

# Changes from Maki

Caudra diverged from [Maki](https://github.com/tontinton/maki) after commit [`d2d6e75`](https://github.com/tontinton/maki/commit/d2d6e757983551776a023f0dece7662aa03e3d95). Both projects continued from that point, so this page records work added on the Caudra branch rather than comparing every later release from both projects.

Maki provided the native Rust TUI, Lua plugin system, provider integrations, MCP, ACP, skills, image input, and the original `index`, `code_execution`, and `task` tools. Caudra retains that foundation and changes how longer work is completed, steered, recovered, and authorized.

## Evidence-gated completion goals

**What changed:** [`/goal <condition>`](/docs/commands/#completion-goals) keeps a session working until a separate evaluator finds evidence that the condition is met. The evaluator has no tools, uses a private transcript copy, and does not add its messages to the conversation. An unmet condition starts another work turn.

**Why:** An agent can produce a plausible final answer before it has verified the requested outcome. A completion condition gives the run an explicit definition of done and checks it against the work already recorded.

## Durable task agents

**What changed:** [`task` agents](/docs/commands/#tasks) have stable IDs, separate transcripts, queued guidance, and histories that survive session reloads. Later calls can continue a task by ID. System prompt profiles can choose the task prompt, model, reasoning, and plan or build capability.

**Why:** Specialist context should survive a long investigation without filling the main conversation. Durable histories let a task resume where it stopped instead of rebuilding its context from a summary.

## Queue, guide, and replace

**What changed:** [Queue and steering](/docs/queue/) separates three intents. A prompt can wait for the next run, guide the active run at a safe turn boundary, or cancel and replace it. Pending entries remain editable and survive session reloads.

**Why:** Steering should not mutate a provider response or tool call already in flight. Explicit delivery modes let users redirect work without losing queued requests or repeating context.

## Branchable and reversible sessions

**What changed:** [Sessions](/docs/sessions/) use parent-linked history, so a fork or revert keeps the abandoned path available. Revert can restore the conversation, workspace files, or both. Session leases reject a second active writer for the same session ID.

**Why:** Experiments need a safe return path. Parent-linked history preserves alternatives, crash-safe file restoration protects the workspace, and single ownership prevents two runtimes from silently overwriting one session.

## Exact and durable permissions

**What changed:** [Reusable permission decisions](/docs/permissions/) bind to host-validated input, typed resources, tool implementation identity, authority, and lifetime. Exact calls remain the default. Trusted tools can offer explicit filesystem, URL, shell, search, or MCP scopes.

**Why:** A rule based only on a tool name can authorize a changed implementation or a wider argument than the user reviewed. Structured authority reduces repeated prompts without turning one approval into hidden broad trust.

## Decoupled native tools with recoverable output

**What changed:** Caudra moves first-party file, web, shell, index, isolated Python, and execution-environment tools behind Workcell's protocol-neutral typed contracts. Workcell owns schemas, validation, execution bounds, atomic file changes, network policy, subprocess cleanup, cancellation, and the bundled Monty worker lifecycle. Caudra owns registration, authorization, retained session output, and model or UI presentation. Release builds pin an exact Workcell revision. Long successful results can be searched or read later with [`tool_output_grep` and `tool_output_read`](/docs/tools/).

**Why:** Separating execution contracts from the agent and protocol layers keeps validation and cleanup consistent across the TUI, headless mode, and ACP. Workcell can evolve and be tested independently, while the pinned release source remains reproducible. Bounded results control context growth, and retained output keeps omitted evidence available after truncation, compaction, cancellation, and session forks.

## Model-aware reasoning and workload roles

**What changed:** [Reasoning controls](/docs/providers/) resolve against the selected model's declared toggle, effort levels, or token limits. Chat, Goal, Compact, Title, Fast, Balanced, and Best are separate model purposes with their own assignments and fallbacks.

**Why:** Provider-wide reasoning tables can advertise unsupported settings and send invalid requests. Model-declared controls keep the UI and request payload aligned with the model that will receive them.

## Source-faithful terminal review

**What changed:** [Review](/docs/review/) attaches notes to exact reply passages and gathers them into one editable review prompt. Quotes preserve the Markdown source that produced the selected rows.

**Why:** Feedback is more useful when it points to the exact source that needs correction. Collecting notes before sending also lets the user revise the complete review before it reaches the model.

## Rich Markdown rendering

**What changed:** Caudra adds source-aware [Markdown rendering](/docs/markdown/) for Unicode LaTeX, strict local Mermaid flowcharts, wide-diagram panning, and validated HTTP links. Unsupported Mermaid syntax remains a highlighted code fence. Drag selection and `Ctrl+X y` recover the original Markdown and LaTeX rather than copying rendered terminal glyphs.

**Why:** Rich terminal output should remain readable without a browser, subprocess, or download. Source provenance prevents a table, equation, or diagram from becoming unusable when copied back into a file or prompt.

## Workbench beside the transcript

**What changed:** Caudra adds a [workbench](/docs/workbench/): a tree explorer, a tabbed editor with undo and find, a source control view backed by gix, and a project-wide content search, all in one full-screen layout on `Ctrl+X w`. It watches the project while it is open, so tabs follow what the agent writes, and `Ctrl+X Enter` hands the current file, line, or selection to the composer as a reference.

**Why:** Reading a diff or checking a line usually meant leaving the session for an editor and losing the thread. Keeping the files in reach also makes references exact, so the agent gets `@src/lib.rs:L42-L58` instead of a description of where to look.

## Mouse controls across the TUI

**What changed:** Building on Maki's wheel scrolling and drag selection, Caudra routes pointer input to native pickers, queue actions, delivery modes, footer controls, paste tokens, task cards, links, review passages, and message actions. Right-click or a half-second left hold opens actions for a message. A control activates only when press and release reach the same target, while dragging cancels activation and preserves text selection.

**Why:** Pointer support needs to behave consistently across the complete interface. Semantic hit targets and overlay-aware routing make controls usable with a mouse without turning an attempted text selection into an accidental action.

## Claude subscription sign-in

**What changed:** `caudra auth login anthropic` provides experimental browser OAuth for Claude subscriptions, secure token refresh, logout, and quota windows through `/usage`. [The provider reference](/docs/providers/#anthropic) documents the terms limitation and API-key alternative.

**Why:** Users with an existing Claude subscription can authenticate without manually handling an API key and can see the account limits that affect an active session.

## Independent release identity

**What changed:** Caudra starts a separate `0.1.0` release line with its own executable, crates, configuration paths, Lua namespace, telemetry namespace, repository, installers, site, and visual identity. The rename is a hard break without runtime Maki aliases.

**Why:** An independent fork needs one unambiguous source for releases, issues, documentation, examples, and security fixes. A clean boundary also prevents configuration or telemetry from silently crossing between the two projects.
