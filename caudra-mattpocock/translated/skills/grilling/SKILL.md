---
name: grilling
description: Grill the user relentlessly about a plan, decision, or idea. Use when the user wants to stress-test their thinking, or uses any 'grill' trigger phrases.
---

Interview the user relentlessly until you reach a shared understanding. Map this as a **design tree**: every decision branches into the decisions that hang off it.

Work the tree in **rounds**. The **frontier** is every decision whose prerequisites are already settled: the questions you can ask _now_ without guessing at answers you haven't heard yet. Ask the whole frontier in one round, as one `question` tool call with one question per frontier decision, then wait for the user's answers before the next round.

Shape each question so the user answers in their own words as easily as they pick:

- `header`: the question title, at most 30 characters.
- `question`: the full question, as many sentences as it needs to stand alone.
- `options`: your recommended answer first, its label ending in "(Recommended)", and its `description` saying why. Add another option only for a genuine alternative the user is likely to weigh, never a filler or a catch-all: the tool always adds a box for the user to type their own answer, and an open question with one recommended option is a good question.

Word each question so accepting the recommended option accepts your recommended answer.

Use `question` when it is available and has an answer channel, regardless of the frontend. If it is unavailable or reports no answer channel, ask the round in plain text, numbered, each question followed by your recommended answer:

```
**Q1** - **<question title>**: <question body>

Recommended: <your recommended answer>
```

Each round the user answers reshapes the tree: settled decisions push the frontier outward and unblock questions that depended on them. Recompute the frontier and ask the next round. A question whose answer depends on another question still open in this round belongs to a _later_ round, not this one.

Deferred, skipped, or dismissed questions remain unresolved. A question ID is not an answer, and your recommendation is not consent. Do not re-ask a pending question or guess its answer. Continue only work independent of it; if none remains, say what answer you need and end the turn. Delivered answers reopen the relevant branch. Set `background: true` only when that parameter is available and the answer is not needed for your next steps.

Finding _facts_ is your job, never the user's. Use direct tools for small lookups. Delegate substantial independent exploration with `task`, `mode: "plan"`, and `background: true`. A running exploration is an unsettled prerequisite: its report arrives by itself, so do not poll or repeat the work. Ask the rest of the frontier while it runs. The _decisions_ are the user's: put each to them and wait.

The session is done when the frontier is empty: every branch of the design tree visited, nothing left silently assumed. Do not act on it until the user confirms you have reached a shared understanding.
