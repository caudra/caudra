---
name: retro
description: "Conduct a retrospective on a coding session."
disable-model-invocation: true
---

The user has asked for a **retrospective**. You are suggesting improvements to the coding agent's **environment** to improve future runs. This is recommend-only: report proposals, not edits to memory, instructions, skills, checks, or configuration. Implementation needs a separate request.

## Steps

1. Load `mattpocock/writing-for-agents` with the `skill` tool for the writing style guide.

2. Use the visible conversation for the session the user specifies, defaulting to the current one. Recover missing evidence with targeted history queries, not an exhaustive search: in `code`, read `help("history")` first, use `await history.search(query, scope="session")` or `scope="project"`, then `await history.read(hit["ref"], around=2)` on relevant returned hits. History establishes what happened, not what files contain now; read current files before recommending changes. Search existing native memory and inspect relevant instruction files, including learned blocks, so proposals improve existing knowledge rather than duplicate it. Use learning output already available; do not require a search of every past session.

3. Look for candidates for improvement in these categories.

- **Navigation**: how easy was it for the agent to find the right files? Are there hidden dependencies between files? Would a **navigation pointer** make it easier? _Use when_ the session took a long time to find a piece of information.
- **Automated checks**: are there automated checks that could catch errors the agent made? Linting, typing, tests, filesystem linters? Read the repo's own check command first (its `package.json`/build-tool `lint`/`check` scripts, its CI workflow), so a check that already exists but sits unwired or silently broken is the finding, not a reinvention. A repo with no **guardrail** (no pre-commit hook and no CI job running its lint/typecheck/test command) is itself a finding: an un-linted repo is a standing missed opportunity, not a neutral default. _Use when_ the agent made a mistake an automated check could have caught, or the repo has no guardrail at all.
- **Coding standards**: should the **reviewer agent** be given a new rule to enforce? Should an existing rule be removed or clarified? Classify the violation first: a **mechanical** one (a fixed syntactic pattern, a banned API, an import shape, a file-location rule) gets a deterministic check, full stop: a custom rule in the repo's own linter, a new pre-commit hook, or a new CI job, whichever the repo's language and existing guardrail make cheapest. Recommend the check over writing the rule. Reserve `CODING_STANDARDS.md` for genuine **judgement calls** (cross-file consistency, "matches the surrounding style," anything no guardrail could ever substitute for). _Use when_ the reviewer agent failed to catch a mistake.
- **Global AGENTS.md**: are there any steering instructions that should be moved to coding standards (or automated checks) instead? _Use when_ the AGENTS.md file is particularly large - in the repo OR the user's global scope.
- **Tool economy**: did the agent make expensive tool calls that could be streamlined? Is there any custom tooling (CLI's, MCP servers) that is particularly token-inefficient? _Use when_ the agent made an expensive tool call.
- **No-ops**: look for instructions in steering files that don't modify the agent's behavior. _Use when_ the steering files are large and unwieldy.
- **Information access**: look for opportunities to increase the agent's access to information. Teeing dev server logs, readonly access to third-party services. _Use when_ a crucial piece of information was not available to the agent.

4. Present these candidates to the user, in order of severity, with evidence, the proposed destination, and any uncertainty. Use the knowledge destination rules in `writing-for-agents`; distinguish a new recommendation from something memory or continual learning already captured. Redact secrets and unnecessary personal information from examples.

## Reference

### Implementation vs Review

Remember that all work goes through two stages: implementation and review. The implementation agent has the most **context pressure**. They are responsible for exploration, writing code, and debugging failures.

The review agent starts with a bounded diff and review criteria. It still reads enough surrounding code and source evidence to validate findings, but does not need the implementation agent's whole exploration history.

Keep implementation guidance concise and put detailed review criteria in the review brief. Both agents must follow applicable project standards; a later review is not a reason to ignore them during implementation.

### Files

You have access to several files in the repo:

- `CLAUDE.md`/`AGENTS.md`: these files are pushed to the context window of any agent working in this repo. They should be used incredibly sparingly, usually only for **navigation pointers** to other files.
- `CODING_STANDARDS.md`: this file is read during review, not implementation. Add **navigation pointers** to docs folders if the standards file gets more than 1,000 lines long.
- Docs: use docs as references files, pointed to by other files. Look for existing docs before writing new ones.
- Skills (`.caudra/skills/<name>/SKILL.md`): use skills for reusable procedures with discovery triggers, not as a substitute for all docs. Commands (`.caudra/commands/<name>.md`) are for what only the user invokes. Follow the advice in the `writing-for-agents` skill.
