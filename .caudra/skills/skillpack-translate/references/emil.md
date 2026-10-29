# emil: from a generic skills repo to Caudra

emilkowalski/skills is Emil Kowalski's design-engineering pack, written for any harness that installs skills (Claude Code, Cursor, skills.sh). The crate is `caudra-emil`, synced with `make sync-emil ARGS='--source <emilkowalski/skills checkout>'`. `caudra-emil/pack.json` lists each shipped skill as its own root with the same path upstream and here, so a path to `--mark` reads `skills/animate/SKILL.md`. The term list is `FOREIGN_TERMS` in `caudra-emil/src/lib.rs`.

`caudra-emil/caudra/` holds pages Caudra adds, served as `emil/caudra/<file>`. `visual-check.md` is the one page the skills share for looking at rendered UI.

## What ships

Every skill in `skills/`, plus `ui-cheatsheet.md` and `performance-cheatsheet.md` as shared pages and `LICENSE`. Never `README.md` (installer and newsletter text; the index replaces it), `.pl`, or the repository dotfiles. List root files under `files`, never a root `trees` entry, which would sweep in the dotfiles. A new upstream skill needs its own root in `pack.json`, so adding one is a deliberate decision. Every skill a shipped page loads must ship too; the check fails otherwise.

## Who starts each skill

Upstream marks `prototype`, `review-animations`, and `pick-ui-library` with `disable-model-invocation: true`. The translations add it to `improve-animations` (a multi-phase audit that writes plans and can run an executor) and `break-ui` (it adds fixtures and dev-only toggles to the project). Keep the flag on all five. A page the model may load never loads one of these five; it suggests the command instead, such as "suggest `/emil:pick-ui-library`". It may still read their reference leaves, such as `emil/review-animations/STANDARDS.md`.

## Vocabulary

| Upstream | Caudra |
| --- | --- |
| a sibling named in backticks, "that's `review-animations`" | `emil/review-animations`, loaded with the `skill` tool. For a user-only sibling, the command `/emil:review-animations`. |
| an invocation the user types, "`improve-animations plan <x>`" | `/emil:improve-animations plan <x>` |
| a link to a sibling's file, `[AUDIT.md](AUDIT.md)` | keep the relative link; in a task brief, the page address `emil/improve-animations/AUDIT.md`, never a filesystem path |
| subagents, "fan out read-only subagents" | `task` calls in one `batch`, each `mode: "plan"`, returning findings only. Stay within `[agent] max_concurrent`. |
| an executor subagent in an isolated worktree | a `task` with `mode: "build"` whose brief tells it to `git worktree add` a new branch under the scratch directory and work only there |
| "if running non-interactively" | "if `question` is unavailable or has no answer channel" |
| "stop and wait for the user to select" | one `question` call, `multiSelect` where several apply, the recommended option first ending in "(Recommended)" |
| "if browser tooling is available, screenshot" | follow `emil/caudra/visual-check.md` |
| a general review skill | `/review` |
| `plans/`, `plans/README.md` | the session plan (see Changes of method) |
| "useful sweeps" with grep | `file_grep`; `source_code_context`, `file_index`, and `source_code_refs` to find where a component, hook, or token is defined and used. Graph counts are lower bounds. |
| look up a library's docs or maintenance | `websearch` and `webfetch` |

`argument-hint` stays: it becomes the `/emil:<name>` command's argument hint. Install commands such as `npx expo install` are the project's own tooling and stay.

## Changes of method

Beyond vocabulary, these are deliberate. Keep them when upstream changes the surrounding text:

- **Opening.** The canned "I'm ready to …" reply goes. Invoked with no task, the skill asks what to work on through `question` (plain text when there is no answer channel); invoked with a task, it starts. A deferred, skipped, or dismissed answer stays unresolved.
- **Defaults, not decrees.** Emil's curves, durations, and taste are the defaults to reach for; a project's own established motion language or an explicit user choice wins where it is coherent. A finding needs evidence at a file and line: "default to flagging" becomes "flag what the evidence shows". Accessibility (reduced motion, focus, contrast, keyboard) ranks above polish. Counts upstream demands become ceilings ("up to five"), and an empty result is valid.
- **Technical accuracy.** Do not claim every CSS animation runs off the main thread: `transform`, `opacity`, and some `filter`/`clip-path` animations can be composited; animating layout or paint properties cannot. Do not claim the GPU is always used. Reduced motion means less or gentler motion, so a short opacity fade remains a valid replacement for movement.
- **Plan mode.** Read-only skills (`find-animation-opportunities`, `review-animations`, `animation-vocabulary`, the diagnosis in `animation-performance`, the audit in `improve-animations`) run as written. A skill that edits code puts the intended change into the session plan: the main agent reads it with `plan`, preserves unrelated sections, and saves the complete revised content, then waits for Build mode. Tasks return drafts and never write the plan.
- **Plans from `improve-animations`.** Its numbered plans go into the session plan in both modes, one section per plan in `PLAN-TEMPLATE.md`'s shape, with the index table `plans/README.md` held. Write `plans/NNN-*.md` and `plans/README.md` in the repository only when the user asks for them, in Build mode. `reconcile` reads whichever exists.
- **Verification.** Run the project's own typecheck, lint, tests, and build through `shell`. Evidence is a terminal result, not an admission receipt. Feel, smoothness, and real-device behaviour are steps for the user; report them as unverified until the user says what they saw. `view_image` reads a still image; it cannot prove motion is smooth.
- **Versions first.** Before applying a recipe, read the installed versions (package manifest, Expo SDK, Reanimated, Sonner, the Swift toolchain). Never upgrade a dependency unasked. Treat dated version tables in `write-swift` as dated.
- **Scope of a review.** `review-animations` names its scope before it starts: a committed range, or staged, unstaged, and untracked work. It is read-only.
- **Prototypes.** Variants live in a dev-only route or story the project already supports, or in a standalone HTML file in the scratch directory. The choice and the promotion go through `question`. Cleanup removes only what the skill created.
- **Quotes.** Short quotes from Apple's WWDC talks and from Paul Graham stay verbatim, each attributed to its source.
