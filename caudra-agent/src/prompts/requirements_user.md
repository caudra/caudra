Extract the requirements from the session below. Each `[user N]` block is one message the user sent, oldest first. Each `[agent]` block is the reply the user was answering with the message after it. Each `[question]` block is a question the agent asked, followed by the `[answer]` the user gave. An `[earlier requirements]` block, when present, is the list extracted before the oldest messages were cut.

<session>
{transcript}
</session>

Output exactly the Markdown structure inside <template>, with the section order unchanged. Do not include the <template> tags in your response.

<template>
## Requirements
- [what the user asked to be built, changed, fixed, or produced; `(was: …)` when a later message replaced an earlier ask]

## Constraints
- [rules, preferences, approaches ruled out, style and technology constraints, things that must not change, or "(none)"]

## Decisions
- [choices the user made or handed to the agent, including every answer to a question, with the alternative it was chosen over when one was offered and `(was: …)` when it replaced an earlier choice, or "(none)"]

## Open questions
- [questions asked but not answered, and points the user left ambiguous, or "(none)"]
</template>

Rules:
- Keep every heading even when the section is empty; write "(none)" rather than dropping it.
- Terse bullets, never prose paragraphs.
- Reproduce file paths, symbols, commands, error strings, URLs, and identifiers exactly.
- One bullet per position: the current one, never an earlier one on its own.
- Do not mention this extraction or the format of the material.
