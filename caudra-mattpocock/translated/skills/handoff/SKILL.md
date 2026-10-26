---
name: handoff
description: Compact the current conversation into a handoff document for another agent to pick up.
argument-hint: "What will the next session be used for?"
disable-model-invocation: true
---

Write a handoff document summarising the current conversation so a fresh agent can continue the work. In Plan mode, return it in the response by default; leave the session plan intact. Only if the user asks to store it there, read the plan first and preserve unrelated sections; only the main agent writes the plan, while a task returns a draft. In Build mode, save a portable Markdown file to the scratch directory (the `Scratch directory` path in the Environment block, `$TMPDIR` in shell commands) - not the current workspace - and tell the user its path.

Start from the visible conversation. If a decision or result needed for continuation is missing, use targeted history recovery in `code`: read `help("history")` first, then `await history.search(query, scope="session")` (or `scope="project"` for related sessions), and `await history.read(hit["ref"], around=2)` for relevant returned hits. History is evidence of earlier work, not current file truth: verify paths, diffs, and status against the workspace.

Include the goal, constraints and authorization boundaries, decisions, remaining work, and the next safe step. Record the repository/worktree/branch and relevant artifact paths. For delegated work, carry task IDs, owners, scope, and observed lifecycle state (running, completed, failed, cancelled, or unknown), plus pending reports. A launch receipt is not completion: reports arrive automatically; do not poll or relaunch pending work. Resume a settled task by its `task_id` only where that task is accessible; otherwise transfer its brief and verified result. Separate reported outcomes from verified ones, with checks run, results, failures, and checks still needed.

Include a "suggested skills" section in the document, naming which skills the next agent should load with its skill tool, such as `mattpocock/tdd`.

Do not duplicate content already captured in durable artifacts (specs, plans, ADRs, issues, commits, diffs). Reference them by path or URL instead. For session-local plans or tool output the next agent may not reach, include the minimal continuation-critical excerpt and label its source and verification state.

Redact secrets and unnecessary personal information, including credentials in URLs, logs, commands, and recovered history. Name the required credential or configuration source, never its value.

If the user passed arguments, treat them as a description of what the next session will focus on and tailor the doc accordingly.
