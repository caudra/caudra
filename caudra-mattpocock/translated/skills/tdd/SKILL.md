---
name: tdd
description: Test-driven development. Use when the user wants to build features or fix bugs test-first, mentions "red-green-refactor", or wants integration tests.
---

# Test-Driven Development

TDD is the red → green → refactor loop. This skill is the reference that makes that loop produce tests worth keeping: what a good test is, where tests go, the anti-patterns, and the rules of the loop. Every section applies on every cycle: consult them before and during the loop, not after.

When exploring the codebase, read `GLOSSARY.md` (if it exists) so test names and interface vocabulary match the project's domain language, and respect ADRs in the area you're touching.

In Plan mode, read the existing session `plan` first. Only the main agent merges proposed seams and test slices into it, preserving existing scope and decisions; tasks return drafts. Writing tests or implementation requires Build mode (the user switches with Tab). Once the seams and scope are agreed, continue red → green → refactor without asking for approval each slice.

## What a good test is

Tests verify behavior through public interfaces, not implementation details. Code can change entirely; tests shouldn't. A good test reads like a specification: "user can checkout with valid cart" tells you exactly what capability exists, and it survives refactors because it doesn't care about internal structure.

See [tests.md](tests.md) for examples and [mocking.md](mocking.md) for mocking guidelines.

## Seams: where tests go

A **seam** is the public boundary you test at: the interface where you observe behavior without reaching inside. Tests live at seams, never against internals.

**Test only at pre-agreed seams.** Before writing any test, write down the seams under test and confirm them with the user. No test is written at an unconfirmed seam. You can't test everything, so agreeing the seams up front is how testing effort lands on the critical paths and complex logic instead of every edge case.

When available, use `source_code_context` to locate relevant behavior, `file_index` before focused reads, and `source_code_refs` to inspect callers and existing tests at candidate seams; otherwise use `file_glob`, `file_grep`, and `file_read`. Counts are lower bounds: missing dynamic calls or tests are not proof of absence.

Ask with the `question` tool: "What's the public interface, and which seams should we test?" Give each proposed seam a one-line note on what it catches and what it misses. If the tool or its answer channel is unavailable, present the question in plain text and say which decision is needed to continue. Unanswered, deferred, skipped, or dismissed questions do not authorize dependent work. Reuse seams already agreed for this scope; ask again only when the seam or scope changes.

When the shape of that interface is itself in question (how deep the module is, where the seam belongs, what the interface should expose), load `mattpocock/codebase-design` with the `skill` tool for the vocabulary. It is the shared source of the module, interface, depth, seam, adapter, leverage and locality terms, and it is a reference to consult, not a session to run.

## Anti-patterns

- **Implementation-coupled**: mocks internal collaborators, tests private methods, or verifies through a side channel (querying the database instead of using the interface). The tell: the test breaks when you refactor but behavior hasn't changed.
- **Tautological**: the assertion recomputes the expected value the way the code does (`expect(add(a, b)).toBe(a + b)`, a snapshot derived by hand the same way, a constant asserted equal to itself), so it passes by construction and can never disagree with the code. Expected values must come from an independent source of truth: a known-good literal, a worked example, the spec.
- **Horizontal slicing**: writing all tests first, then all implementation. Bulk tests verify _imagined_ behavior: you test the _shape_ of things rather than user-facing behavior, the tests go insensitive to real changes, and you commit to test structure before understanding the implementation. Work in **vertical slices** instead: one test → one implementation → repeat, each test a **tracer bullet** that responds to what the last cycle taught you.

## Rules of the loop

- **Red before green.** Write the failing test first, then only enough code to pass it. Don't anticipate future tests or add speculative features.
- **Observe both results.** Shell admission or a running-job receipt is not a test result. Wait for terminal completion and verify red fails for the intended behavior and green passes. Use `tool_output` for truncated evidence only when a retained output handle is provided; otherwise report the missing evidence, never invent a handle. Report blocked or pending checks rather than claiming success.
- **One slice at a time.** One seam, one test, one minimal implementation per cycle.
- **Refactor on green.** Once a slice passes, tidy what it touched (names, duplication, the code behind the seam) with every test staying green, then start the next red. Keep it to that slice: restructuring across modules belongs to review (`mattpocock/code-review`), not the loop.
