---
name: improve-codebase-architecture
description: Scan a codebase for deepening opportunities, present them as a visual report, then grill through whichever one you pick.
disable-model-invocation: true
---

# Improve Codebase Architecture

Surface architectural friction and propose **deepening opportunities**: refactors that turn shallow modules into deep ones. The aim is testability and AI-navigability.

This command is _informed_ by the project's domain model and built on a shared design vocabulary:

- Load `mattpocock/codebase-design` with the `skill` tool for the architecture vocabulary (**module**, **interface**, **depth**, **seam**, **adapter**, **leverage**, **locality**) and its principles (the deletion test, "the interface is the test surface", "one adapter = hypothetical seam, two = real"). Use these terms exactly in every suggestion, and don't drift into "component," "service," "API," or "boundary."
- The domain language in `GLOSSARY.md` gives names to good seams; ADRs in `docs/adr/` record decisions this command should not re-litigate.

Explore and report read-only in either mode. In Plan mode, read the existing session `plan` first. Only the main agent merges proposed glossary/ADR changes and implementation decisions into it, preserving existing scope and decisions; tasks return drafts. Writing files or implementing a refactor requires Build mode (the user switches with Tab). Once authorized, carry out agreed changes without re-asking each slice.

If `question` or its answer channel is unavailable, present the question in plain text and say which decision is needed to continue. Unanswered, deferred, skipped, or dismissed questions do not authorize dependent work.

## Process

### 1. Explore

**Scope before you scan: YAGNI.** Deepening a module pays off by making future changes to it easier, so put extra weight on the parts of the codebase that have recently changed. Decide *where* to look before you look:

- If the user named a direction (a module, a subsystem, a pain point), take it, and skip the inference below.
- Otherwise, walk back a good stretch of the commit history (`git log --oneline`) to find the codebase's hot spots, the files and areas that keep coming up, and let those paths pull your attention first. If the changes are scattered with no clear hot spot, widen the net.

Read the project's domain glossary (`GLOSSARY.md`) and any ADRs in the area you're touching first.

Then start a `task` with `mode: "plan"` to walk the codebase.

Give it the selected scope and ask it to use `source_code_context` to locate relevant modules, `file_index` before focused reads, and `source_code_refs` to examine callers, coupling, and tests at candidate seams when available; otherwise use `file_glob`, `file_grep`, and `file_read`. Counts are lower bounds, not proof that unlisted dynamic callers or tests do not exist.

Don't follow rigid heuristics; explore organically and note where you experience friction:

- Where does understanding one concept require bouncing between many small modules?
- Where are modules **shallow**, with an interface nearly as complex as the implementation?
- Where have pure functions been extracted just for testability, but the real bugs hide in how they're called (no **locality**)?
- Where do tightly-coupled modules leak across their seams?
- Which parts of the codebase are untested, or hard to test through their current interface?

Apply the **deletion test** to anything you suspect is shallow: would deleting it concentrate complexity, or just move it? A "yes, concentrates" is the signal you want.

### 2. Present candidates as a report

Present the candidates in your reply, in Markdown: one `###` section per candidate, with terminal-supported Mermaid `graph`, `flowchart`, or `stateDiagram-v2` blocks wherever they communicate the structure; not sequence diagrams. Each candidate gets a **before/after visualisation**. Be visual.

When the user asks for a richer, shareable report, also write a self-contained HTML file to the scratch directory as `architecture-review-<timestamp>.html`, following [HTML-REPORT.md](HTML-REPORT.md), and tell them its absolute path. Writing it needs Build mode.

For each candidate, give:

- **Files**: which files/modules are involved
- **Problem**: why the current architecture is causing friction
- **Solution**: plain English description of what would change
- **Benefits**: explained in terms of locality and leverage, and how tests would improve
- **Before / After diagram**: a before diagram and an after diagram, illustrating the shallowness and the deepening
- **Recommendation strength**: one of `Strong`, `Worth exploring`, `Speculative`

End the report with a **Top recommendation** section: which candidate you'd tackle first and why.

**Use GLOSSARY.md vocabulary for the domain, and the `mattpocock/codebase-design` vocabulary for the architecture.** If `GLOSSARY.md` defines "Order," talk about "the Order intake module," not "the FooBarHandler," and not "the Order service."

**ADR conflicts**: if a candidate contradicts an existing ADR, only surface it when the friction is real enough to warrant revisiting the ADR. Mark it clearly in the candidate (e.g. a warning callout: _"contradicts ADR-0007, but worth reopening because…"_). Don't list every theoretical refactor an ADR forbids.

Do NOT propose interfaces yet. After the report, ask the user with the `question` tool: "Which of these would you like to explore?", one option per candidate, your top recommendation first.

### 3. Grilling loop

Once the user picks a candidate, load `mattpocock/grilling` with the `skill` tool to walk the decision tree with them: constraints, dependencies, the shape of the deepened module, what sits behind the seam, what tests survive.

Side effects happen inline as decisions crystallize; load `mattpocock/domain-modeling` the same way to keep the domain model current as you go:

- **Naming a deepened module after a concept not in `GLOSSARY.md`?** Add the term to `GLOSSARY.md`. Create the file lazily if it doesn't exist.
- **Sharpening a fuzzy term during the conversation?** Update `GLOSSARY.md` right there.
- **User rejects the candidate with a load-bearing reason?** Offer an ADR, framed as: _"Want me to record this as an ADR so future architecture reviews don't re-suggest it?"_ Only offer when the reason would actually be needed by a future explorer to avoid re-suggesting the same thing; skip ephemeral reasons ("not worth it right now") and self-evident ones.
- **Want to explore alternative interfaces for the deepened module?** Use the design-it-twice parallel-task pattern of `mattpocock/codebase-design`.
