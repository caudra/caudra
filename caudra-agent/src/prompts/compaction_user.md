Summarize the conversation above so another coding agent can resume the work with no other context.

Output exactly the Markdown structure inside <template>, with the section order unchanged. Do not include the <template> tags in your response.

<template>
## Objective
- [one or two sentences: what the user is trying to accomplish]

## Constraints and Decisions
- [user directives, stated preferences, approaches ruled out, technical decisions and why, or "(none)"]

## Discoveries
- [non-obvious facts learned about the codebase: architecture, conventions, gotchas, or "(none)"]

## Work State
### Completed
- [finished work and verified facts, or "(none)"]

### Active
- [work in progress, partial edits, current investigation, or "(none)"]

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
- Do not mention this summary or that context was dropped.
