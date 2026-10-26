---
name: code-review
description: "Review the changes since a fixed point (commit, branch, tag, or merge-base) along two axes: Standards (does the code follow this repo's documented coding standards?) and Spec (does the code match what the originating issue/spec asked for?). Runs both reviews in parallel tasks and reports them side by side. Use when the user wants to review a branch, a PR, work-in-progress changes, or asks to \"review since X\"."
---

Two-axis review of a committed range and/or working-tree changes, with the scope pinned before review:

- **Standards**: does the code conform to this repo's documented coding standards?
- **Spec**: does the code faithfully implement the originating issue / spec?

Both axes run as **parallel tasks** so they don't pollute each other's context, then this skill aggregates their findings.

Review is read-only in Plan or Build mode; do not change the session plan unless requested. Applying fixes requires Build mode and is separate from this review. If `question` or its answer channel is unavailable, present the question in plain text and say which decision is needed to continue. Unanswered, deferred, skipped, or dismissed questions do not authorize dependent work.

When the spec lives on [the issue tracker](../../caudra/issue-tracker.md), that page says how to find and read it.

## Process

### 1. Pin the scope and fixed point

Distinguish a **committed branch range** from **staged, unstaged, and untracked WIP**; do not silently include or omit either. Honor the supplied scope. If unclear, ask with `question`, recommending the merge-base with the default branch for a branch review, or all WIP for a work-in-progress review.

For a committed range, resolve the supplied ref and `HEAD` to commit SHAs (`git rev-parse --verify '<ref>^{commit}'`). An **exact commit/tag fixed point** means `git diff <base-sha> <head-sha>`; use `git merge-base <ref-sha> <head-sha>` as the base only when branch/merge-base semantics are intended. Three-dot diff chooses a merge-base; it does not mean "since this exact commit." Record the selected base, head, diff command, and `git log <base-sha>..<head-sha> --oneline`.

For WIP, record `git status --short --untracked-files=all`, `git diff --cached` (staged), and `git diff` (unstaged); read the selected untracked files explicitly, since neither diff includes them. Select only the requested categories and paths. For combined branch-plus-WIP review, retain both evidence sets rather than treating `...HEAD` as the whole change.

Give both tasks the same scope record: resolved commits, commands, path filters, WIP categories, and untracked-file list. Validate refs and check all selected evidence for a non-empty change before launching. Avoid concurrent edits; if the worktree changes during review, refresh both tasks' affected evidence. Shell admission is not completion; wait for terminal results. Recover truncated diffs or logs with `tool_output` only when a retained output handle is provided; otherwise report the missing evidence, never invent a handle.

### 2. Identify the spec source

Look for the originating spec, in this order:

1. The spec or path explicitly supplied by the user.
2. The relevant session plan (`plan` with `action: "read"`), linked issues (fetched as the issue tracker page says), and matching spec files in the repo. Use only requirements relevant to the selected scope; ask about conflicts rather than silently choosing.
3. Only if context is still missing, targeted `history.search` / `history.read` through `code` after reading `help("history")`; do not trawl unrelated sessions.
4. If nothing is found, ask the user where the spec is. If they say there isn't one, the **Spec** task will skip and report "no spec available".

### 3. Identify the standards sources

Search the repo for every file that documents how code should be written. When `CODING_STANDARDS.md` or `CONTRIBUTING.md` exists, it must be on the list.

On top of whatever the repo documents, the Standards axis always carries the **smell baseline** below: a fixed set of Fowler code smells (_Refactoring_, ch.3) that applies even when a repo documents nothing. Two rules bind it:

- **The repo overrides.** A documented repo standard always wins; where it endorses something the baseline would flag, suppress the smell.
- **Always a judgement call.** Each smell is a labelled heuristic ("possible Feature Envy"), never a hard violation. Like any standard here, skip anything tooling already enforces.

Each smell reads *what it is* → *how to fix*; match it against the diff:

- **Mysterious Name**: a function, variable, or type whose name doesn't reveal what it does or holds. → rename it; if no honest name comes, the design's murky.
- **Duplicated Code**: the same logic shape appears in more than one hunk or file in the change. → extract the shared shape, call it from both.
- **Feature Envy**: a method that reaches into another object's data more than its own. → move the method onto the data it envies.
- **Data Clumps**: the same few fields or params keep travelling together (a type wanting to be born). → bundle them into one type, pass that.
- **Primitive Obsession**: a primitive or string standing in for a domain concept that deserves its own type. → give the concept its own small type.
- **Repeated Switches**: the same `switch`/`if`-cascade on the same type recurs across the change. → replace with polymorphism, or one map both sites share.
- **Shotgun Surgery**: one logical change forces scattered edits across many files in the diff. → gather what changes together into one module.
- **Divergent Change**: one file or module is edited for several unrelated reasons. → split so each module changes for one reason.
- **Speculative Generality**: abstraction, parameters, or hooks added for needs the spec doesn't have. → delete it; inline back until a real need shows.
- **Message Chains**: long `a.b().c().d()` navigation the caller shouldn't depend on. → hide the walk behind one method on the first object.
- **Middle Man**: a class or function that mostly just delegates onward. → cut it, call the real target direct.
- **Refused Bequest**: a subclass or implementer that ignores or overrides most of what it inherits. → drop the inheritance, use composition.

### 4. Start both tasks in parallel

Use `batch` for both independent foreground `task` calls, each with `mode: "plan"` so neither edits the code, and aggregate the reports they return. Tell both to use `source_code_context` for relevant implementation paths, `file_index` before focused reads, and `source_code_refs` for affected callers/tests when available; otherwise use `file_glob`, `file_grep`, and `file_read`. Graph counts are lower bounds, not proof that unlisted callers or coverage do not exist.

**Standards task brief** should include:

- The shared scope record and commit list from step 1.
- The list of standards-source files you found in step 3, **plus the smell baseline from step 3** pasted in full (the task has no other access to it).
- The brief: "Report, per file/hunk where relevant, (a) every place the diff violates a documented standard: cite the standard (file + the rule); and (b) any baseline smell you spot: name it and quote the hunk. Distinguish hard violations from judgement calls: documented-standard breaches can be hard, but baseline smells are always judgement calls, and a documented repo standard overrides the baseline. Skip anything tooling enforces. Under 400 words."

**Spec task brief** should include:

- The same scope record and commit list from step 1.
- The path or fetched contents of the spec.
- The brief: "Report: (a) requirements the spec asked for that are missing or partial; (b) behaviour in the diff that wasn't asked for (scope creep); (c) requirements that look implemented but where the implementation looks wrong. Quote the spec line for each finding. Under 400 words."

If the spec is missing, skip the Spec task and note this in the final report.

### 5. Aggregate

Validate child findings against the scoped diff, surrounding code, and cited standard or spec. Remove unsupported claims and duplicates within each axis; mark unresolved uncertainty and checks not run. Present the validated reports under `## Standards` and `## Spec` headings. Do **not** merge or rerank findings across axes, because the two axes are deliberately separate (see _Why two axes_).

End with a one-line summary: total findings per axis, and the worst issue _within each axis_ (if any). Don't pick a single winner across axes: that's the reranking the separation exists to prevent.

## Why two axes

A change can pass one axis and fail the other:

- Code that follows every standard but implements the wrong thing → **Standards pass, Spec fail.**
- Code that does exactly what the issue asked but breaks the project's conventions → **Spec pass, Standards fail.**

Reporting them separately stops one axis from masking the other.
