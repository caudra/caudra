---
name: reflect
description: Spawn three parallel review subagents over the active transcript, surface learnings, and route each to a concrete edit on an existing skill. Use when the user says reflect.
disable-model-invocation: true
---

# Reflect

Mine the current conversation for durable learnings, then route them into skill edits.

Invoke when the user says "reflect" or "/pstack:reflect". Skip when the conversation is trivial, off-topic, or already covered by an existing skill the parent followed correctly. One-offs are not learnings.

## 1. Locate the active transcript

Before fanning out, write a tight digest of this conversation: its opening user prompt, each correction the user gave, the decisions with their evidence, and the skills and tools it used. The transcript itself is this session's history, compacted parts and subagents included. In `code`, `history.search(query, scope="session")` finds a moment the digest names and `history.read` returns it in full. A task runs in this session, so its `scope="session"` reaches the same conversation. Do not search `scope="all"` or another project's path. That crosses workspace boundaries and reads private chats from unrelated projects.

## 2. Spawn three reviewers in parallel

One message, three `task` calls with `profile: "builtin"` and `mode: "build"`. A task's mode is capped by the caller's, so the session must be in Build mode. Reviewers need MCP access to look up the tickets, chat threads, and observability traces the transcript references, and plan mode strips most MCP tools.

Each reviewer and the synthesizer name a row of the **Model roles** table below. Set `model` and `thinking` to that row's values, and leave both unset when the row is `inherit`. Change the rows in `/settings` → pstack. If `task` rejects a value, run that spawn as `inherit` and say so. If `task` has no `model` or `thinking` argument, omit both and say so. Pick each subagent's reasoning level for the job, per the Subagents section of the **poteto-mode** skill.

| Lens | Model row | Prompt template |
|---|---|---|
| Judgment | Judgment | `references/judgment-reviewer.md` |
| Tooling | Code | `references/tooling-reviewer.md` |
| Divergent | Judgment | `references/divergent-reviewer.md` |

Pass each template verbatim, substituting the digest where marked. Reviewers return findings in their task's final report.

## 3. Synthesize

One `task` call with `profile: "builtin"`, `model` and `thinking` from the Judgment row, and `mode: "build"`, which again needs the session in Build mode. It spot-verifies citations through MCP, and plan mode strips most MCP tools. Pass `references/synthesizer.md` verbatim, with each reviewer's full output inlined where marked. It returns an Accepted / Rejected / Backlog list.

## 4. Structural enforcement check

Move any Accepted item that a lint rule, script, metadata flag, or runtime check would enforce more reliably to Backlog. See the **encode-lessons-in-structure** principle skill.

## 5. Apply

Present the synthesizer's full Accepted / Rejected / Backlog output and wait for explicit approval before applying any Accepted edit. The user picks the subset and may redirect routings. Skill changes affect every future agent in the org. Do not auto-apply.

File each Backlog item to your team's devex or backlog tracker without waiting. Only the Accepted list waits for approval.

Follow each approved row's Routing exactly:

- Trivial existing-skill edit (a one-line bullet, a tightened sentence, a stale fact corrected): the parent does it directly.
- Substantive existing-skill edit (a new section, a new pattern table, more than ~10 lines): load `caudra-docs/skills` with the `skill` tool, then draft, test, and iterate the edit against it.
- `tune description: <skill path>` (the skill exists but didn't trigger when it should have): load `caudra-docs/skills` with the `skill` tool and rewrite the description, which is what the model reads when it picks a skill, until it would have triggered here.
- `new skill: <kebab-name>`: load `caudra-docs/skills` with the `skill` tool and create the skill from it. Do not invent the shape ad hoc.

If your environment ships a SKILL.md validator, run it on every touched skill before declaring done.

## 6. Summarize for the user

Short list, no preamble:

- Edits applied: `<skill path>`. What changed, one line each.
- New skills created: `<skill path>`. One line each (rare).
- Backlog filed to the devex tracker: `<issue title>` (`<tags>`). One line each.
- Dropped: one line per rejected finding + reason from the synthesizer.
