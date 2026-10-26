# mattpocock: from Claude Code to Caudra

mattpocock/skills is written for Claude Code and the harnesses skills.sh installs into. The crate is `caudra-mattpocock`, synced with `make sync-mattpocock ARGS='--source <mattpocock/skills checkout>'`. `caudra-mattpocock/pack.json` lists the skills Caudra ships, each flattened from `skills/<bucket>/<name>` to `skills/<name>`, so a path to `--mark` reads `skills/tdd/SKILL.md`. The term list is `CLAUDE_TERMS` in `caudra-mattpocock/src/lib.rs`.

`caudra-mattpocock/caudra/` holds pages Caudra adds, served as `mattpocock/caudra/<file>`. `issue-tracker.md` is the one page `to-spec`, `to-tickets`, and `code-review` share for publishing and fetching, in place of upstream's setup skill.

## What ships

Only skills that are high quality and generically useful: the stable `engineering/` and `productivity/` buckets, minus the author's router and setup skills (`ask-matt`, `setup-matt-pocock-skills`), thin or redundant ones (`implement`, `implement-spec`, `research`, `wait-what`), and narrow ones (`prototype`, `wizard`, `triage`, `wayfinder`, `teach`). Never `misc/` (frozen upstream) or `in-progress/` (beta upstream). Adding a skill is a `pack.json` edit, then a sync. Every skill a shipped page loads must ship too; the check fails otherwise.

## Tools

| Claude Code | Caudra |
| --- | --- |
| "Call the Skill tool with X" | load `mattpocock/X` with the `skill` tool and follow it. "Twice, for A and B" loads both. |
| `/x`, the user types it | `/mattpocock:x` |
| a sub-agent, "spawn sub-agents in parallel" | a `task`, with independent parallel calls in `batch`. One that only reads or reviews gets `mode: "plan"`. |
| a background agent, "don't block on it" | `background: true`; its report arrives by itself |
| `AskUserQuestion`, "ask in one exchange" | one `question` call when available with an answer channel, otherwise plain text |
| session logs on this machine | `history.search` and `history.read` in `code` |
| `disable-model-invocation: true` | keep it in the frontmatter: the index lists the skill under "Skills the user runs" |
| `argument-hint` | keep it: it becomes the `/mattpocock:<name>` command's argument hint |
| the OS temp directory, `$TMPDIR` else `/tmp`, `%TEMP%` | the scratch directory |
| a user-invoked skill (in `writing-for-agents`) | an ordinary project command at `.caudra/commands/<name>.md`, invoked as `/project:<name>`, or a user command as `/user:<name>`. Builtin packs separately expose user-only pages through their namespaced commands and index. |

## Grilling

Upstream deliberately asks grilling rounds in plain chat text. Caudra asks them through `question`, by our choice: one call per round, one question per frontier decision, the recommended answer as the first option ending in "(Recommended)", and other options only for genuine alternatives, since the tool always lets the user type their own answer. Use numbered plain text only when the tool is unavailable or has no answer channel, not merely because the frontend is headless. Every other interview step (seams in `tdd` and `to-spec`, the fixed point in `code-review`, the breakdown in `to-tickets`, the send in `to-questionnaire`) uses this capability rule too.

Deferred, skipped, or dismissed answers leave their decisions unresolved. Continue independent work without guessing or re-asking a pending question. End the turn when only blocked work remains. Small factual lookups use direct tools; substantial independent exploration may use background tasks, whose results arrive without polling.

## The issue tracker

Upstream's `/setup-matt-pocock-skills` prerequisite goes: a line saying the tracker "should have been provided" becomes a link to `caudra/issue-tracker.md`. Project instructions are authoritative; the git remote is only a hint to confirm. Choose transport separately: available MCP capabilities discovered through `code`, the project's CLI, or authorized persistent local Markdown. Never invent an MCP tool or require a server. Labels upstream applies unconditionally (`ready-for-agent`) apply only when the project names them. Publishing tickets does not authorize implementing them.

## Changes of method

Beyond vocabulary, these are deliberate. Keep them when upstream changes the surrounding text:

- `tdd`: a **refactor on green** step after each slice, kept to that slice; cross-module restructuring stays with `code-review`.
- `improve-codebase-architecture`: the report is Markdown in the reply, with ```` ```mermaid ```` blocks; the HTML report (`HTML-REPORT.md`) is written to the scratch directory only when the user asks for one.
- `diagnosing-bugs`: a human in the loop is driven through `question`; the HITL script is for steps that must run in the user's own terminal, which they run and paste back, since Caudra's shell has no terminal for them.
- Writing a glossary, an ADR, a questionnaire, a spec, or tickets needs Build mode; in Plan mode it goes into the session plan with the `plan` tool.
- Plan drafts preserve existing content and are written only by the main agent; tasks return drafts. A Plan-mode handoff stays in the reply by default. Publication re-reads the draft and needs authorization, not just a mode switch.
- `grilling` and other interviews respect question deferral and available answer channels. Diagnosis accepts structured user-reported observations, including runs the user already performed, as well as command-based reproduction. Label reported evidence separately from independent verification. A hypothetical assessment does not authorize execution. Load the HITL template as a page before writing a Build-mode copy.
- `code-review` pins the requested committed or working-tree scope and supplies the same scope to both read-only tasks through `batch`. Explicit specs win; the current session plan is a spec source too. Validate findings before reporting, retaining separate Standards and Spec axes.
- Handoffs, retrospectives, specs, and tickets use targeted history recovery only when needed. Read `help("history")`, search, then read returned item references. History establishes prior decisions, not current file state.
- Put current work in the plan/handoff, reusable project-local knowledge in memory, shared conventions in `AGENTS.md`, procedures in skills, and team artifacts in docs or the tracker. Search existing knowledge before proposing duplicates; retro recommends rather than applies changes. Reviewers need enough surrounding source to validate findings, and implementation still follows project standards.
- Use available source intelligence for relevant exploration and seam decisions, with file-search fallback and graph counts treated as lower bounds. Terminal diagrams use supported flowcharts/state diagrams; external PR/HTML renderers keep their own capabilities.
- Verification needs terminal execution results, not admission receipts. Recover truncated evidence through `tool_output` when available. Handoffs preserve running-task ownership and IDs, and never restart pending work blindly.
