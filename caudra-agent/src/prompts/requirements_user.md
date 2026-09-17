Extract the requirements from the user's side of the session below. Each `[user N]` block is one message the user sent, oldest first. Each `[question]` block is a question the agent asked, followed by the `[answer]` the user gave.

<session>
{transcript}
</session>

Output exactly the Markdown structure inside <template>, with the section order unchanged. Do not include the <template> tags in your response.

<template>
## Requirements
- [what the user asked to be built, changed, fixed, or produced]

## Constraints
- [rules, preferences, approaches ruled out, style and technology constraints, things that must not change, or "(none)"]

## Decisions
- [choices the user made, including every answer to a question, with the alternative it was chosen over when one was offered, or "(none)"]

## Open questions
- [questions asked but not answered, and points the user left ambiguous, or "(none)"]
</template>

Rules:
- Keep every heading even when the section is empty; write "(none)" rather than dropping it.
- Terse bullets, never prose paragraphs.
- Reproduce file paths, symbols, commands, error strings, URLs, and identifiers exactly.
- Do not mention this extraction or the format of the material.
