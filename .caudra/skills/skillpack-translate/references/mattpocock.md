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
| a sub-agent, "spawn sub-agents in parallel" | a `task`, several `task` calls in one turn. One that only reads or reviews gets `mode: "plan"`. |
| a background agent, "don't block on it" | `background: true`; its report arrives by itself |
| `AskUserQuestion`, "ask in one exchange" | one `question` call (plain text when headless) |
| session logs on this machine | `history.search` and `history.read` in `code` |
| `disable-model-invocation: true` | keep it in the frontmatter: the index lists the skill under "Skills the user runs" |
| `argument-hint` | keep it: it becomes the `/mattpocock:<name>` command's argument hint |
| the OS temp directory, `$TMPDIR` else `/tmp`, `%TEMP%` | the scratch directory |
| a user-invoked skill (in `writing-for-agents`) | a command in `.caudra/commands/`, since Caudra lists every skill in the catalog |

## Grilling

Upstream deliberately asks grilling rounds in plain chat text. Caudra asks them through `question`, by our choice: one call per round, one question per frontier decision, the recommended answer as the first option ending in "(Recommended)", and other options only for genuine alternatives, since the tool always lets the user type their own answer. Headless runs keep upstream's numbered plain-text format. Every other interview step (seams in `tdd` and `to-spec`, the fixed point in `code-review`, the breakdown in `to-tickets`, the send in `to-questionnaire`) asks the same way.

## The issue tracker

Upstream's `/setup-matt-pocock-skills` prerequisite goes: a line saying the tracker "should have been provided" becomes a link to `caudra/issue-tracker.md`, which resolves it from `AGENTS.md`, then the git remote, then local Markdown, and asks once. Labels upstream applies unconditionally (`ready-for-agent`) apply only when the project names them.

## Changes of method

Beyond vocabulary, these are deliberate. Keep them when upstream changes the surrounding text:

- `tdd`: a **refactor on green** step after each slice, kept to that slice; cross-module restructuring stays with `code-review`.
- `improve-codebase-architecture`: the report is Markdown in the reply, with ```` ```mermaid ```` blocks; the HTML report (`HTML-REPORT.md`) is written to the scratch directory only when the user asks for one.
- `diagnosing-bugs`: a human in the loop is driven through `question`; the HITL script is for steps that must run in the user's own terminal, which they run and paste back, since Caudra's shell has no terminal for them.
- Writing a glossary, an ADR, a questionnaire, a spec, or tickets needs Build mode; in Plan mode it goes into the session plan with the `plan` tool.
