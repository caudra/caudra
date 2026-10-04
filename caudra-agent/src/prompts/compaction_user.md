Summarize the conversation above so another coding agent can resume the work with no other context.

Output exactly the Markdown structure inside <template>, with the section order unchanged. Do not include the <template> tags in your response.

<template>
## Objective
- [what the user is trying to accomplish and their stated acceptance criteria]

## Constraints and Decisions
- [user directives, stop/approval conditions, stated preferences, approaches ruled out, technical decisions and why, or "(none)"]

## Discoveries
- [non-obvious facts learned about the codebase: architecture, conventions, gotchas, or "(none)"]

## Work State
### Completed
- [finished work and verified facts, or "(none)"]

### Active
- [work in progress, partial edits, current investigation, or "(none)"]

### Delegated work
- [task ID or resumable workflow ID, assigned objective and scope/constraints, expected result or dependency, parent's next step after delivery, and last observed status; or "(none)"]

### Blocked
- [blockers, failing commands with their exact error, unresolved questions, or "(none)"]

## Next Move
1. [immediate concrete action, or "(none)"]
2. [action after that, if known]

## Relevant Files
- `path`: [why it matters; what changed or still needs to change]

## Todo List
- [if a todo list was in use, repeat every item verbatim with its current status, otherwise "(none)"]
</template>

Rules:
- Keep every heading even when the section is empty; write "(none)" rather than dropping it.
- Terse bullets, never prose paragraphs.
- Reproduce file paths, symbols, commands, error strings, URLs, and identifiers exactly. Never paraphrase an identifier.
- If a tool result was truncated and its output ID was given, carry that ID into the summary so the full output can be re-read.
- Record what was tried and failed, and why, so it is not retried.
- Preserve the latest user-stated acceptance criteria and stop/approval conditions. Do not invent requirements or turn unaccepted agent proposals into user requirements.
- Preserve unfinished delegated work. Record completion only when supported by a final result, not silence or an acknowledgment.
- Keep important results already delivered, without copying child transcripts. Preserve full-output handles only when omitted content is still needed.
- Delegation status is last observed. The latest host status observation supplies current execution state, but does not replace assignment constraints or actual result reports. Do not invent a live task ledger.
- Do not mention this summary or that context was dropped.
