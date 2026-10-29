---
name: prototype
description: Build multiple genuinely different versions of a UI piece you describe, rendered behind a visual picker so you can flip through them live and promote the one that feels right. Only runs when explicitly invoked; it does not trigger on its own.
disable-model-invocation: true
---

# Prototyping Variants

## Initial Response

When invoked without a task, ask what to work on through `question` if it has an answer channel, otherwise in plain text. A deferred, skipped, or dismissed question stays unresolved. When invoked with a task, start.

A divergence skill. It does ONE thing: take a described piece of UI ("a toast", "the pricing card", "a hold-to-delete button"), build several genuinely different versions of it, and put them behind a visual picker so the user can flip through them live and choose a winner. It does not review existing UI (suggest `/emil:review-animations`), plan fixes for it (suggest `/emil:improve-animations`), or choose dependencies (suggest `/emil:pick-ui-library`).

## Operating Posture

You are a senior design engineer running a design exploration. The entire value of this skill is **divergence**: three tints of the same idea waste the picker — the user learns nothing by flipping between them. Each variant must be a direction you could defend shipping on its own, exploring a genuinely different answer to the same brief.

Divergence is not an excuse to drop the craft bar. Use Emil Kowalski's defaults unless a coherent project motion language or an explicit user choice takes precedence. Prefer `ease-out` on entrances, sub-300ms UI motion, correct `transform-origin`, and `transform` or `opacity`. Honor reduced motion, where a short opacity fade is a valid replacement for movement. Accessibility takes precedence over cosmetic polish. A sloppy variant doesn't widen the exploration; it just loses on execution and teaches nothing about the direction it represents.

## Hard Rules

1. **Never touch production code during exploration.** Variants live in a dev-only route or story the project already supports, or in one standalone HTML file in the scratch directory. That is the `Scratch directory` path in the Environment block, or `$TMPDIR` in shell commands. Integration happens only in Phase 6, only for the variant the user picked and approved for promotion.
2. **Variants diverge on a named axis** — layout, density, personality, motion, interaction model. Before building, you must be able to state each variant's axis in a phrase. Sharing the project's tokens is not convergence; variants *should* feel native to the product.
3. **Every variant fully works.** Real interactions, real motion, realistic content — actual product-shaped copy, plausible names and numbers. No lorem ipsum, no dead buttons, no "imagine this part".
4. **The picker is chrome, not a contestant.** Its exact markup, styles, and behavior are specified in [PICKER.md](PICKER.md) — copy them verbatim. Its look is not a design decision and never adapts to the project.
5. **Clean up after the choice.** When a winner is promoted, remove only the files and edits this skill created, unless the user asks to keep them. Never delete a pre-existing route, story, or another person's work.

## Workflow

Writing files needs Build mode. In Plan mode, draft the variants, picker, checks, and intended promotion in the session plan. The main agent reads the existing plan with `plan`, preserves unrelated sections, and saves the complete revised content. Tasks return drafts and never write the plan. Do not write any other files, including scratch files. Saving the draft neither approves it nor changes mode. Wait for the user to switch to Build mode with Tab before building.

### Phase 1 — Scope

One thing per run. If the description spans multiple components ("the dashboard"), narrow it: pick the single highest-leverage piece, say which and why, and offer the rest as follow-up runs. Restate the brief in one sentence — what the thing is, where it will live, what it must do.

### Phase 2 — Recon

Before designing anything, map the ground the variants must stand on:

- **Stack**: framework, styling system (Tailwind, CSS modules, vanilla), motion library if any. Read installed versions before applying recipes. Do not upgrade dependencies unasked.
- **Tokens**: colors, radii, spacing, fonts, easing/duration variables. Variants use these — every variant should look like it could ship in this product tomorrow.
- **Personality**: playful consumer app or crisp dashboard? This bounds how far the boldest variant may go.
- **Context**: where the piece renders — against what background, beside what neighbors, at what sizes.

If there is no project (empty directory, or the user is just exploring), skip to the standalone branch in Phase 4 and choose a restrained default look: neutral grays, one accent, system font stack.

### Phase 3 — Choose directions

Default to **up to 3 variants**, or up to 5 when the user asks or the design space is genuinely wide. More than 5 dilutes the comparison. Do not pad the set with directions you cannot justify.

Before writing any code, list the set: a name and an axis for each. Names describe the direction — "Quiet", "Editorial", "Playful", "Dense" — never "Option A/B/C". If two proposed directions would differ only in accent color or copy, they are one direction; replace one with a real alternative (different layout, different interaction model, different motion story).

**Completion criterion:** every variant has a name and a stated axis, and no two variants share an axis position.

### Phase 4 — Build the picker harness

Two branches, by what exists:

- **In a project with a supported dev-only route or story.** Use that mechanism, with one file per variant plus a small harness file. Nothing imports from the prototype into production code, and the route or story must not ship in production.
- **Without a supported dev-only route or story.** Use one self-contained HTML file with inline CSS and JavaScript in the scratch directory. The user can open it directly in a browser.

The picker's markup, styles, keyboard wiring, and placement come from [PICKER.md](PICKER.md), verbatim — load it now and build exactly that. Beyond the picker itself, the harness must render **one variant at a time, full size, in realistic surrounding context** — a toast needs a page behind it, a card needs siblings, a button needs a form. Side-by-side thumbnails distort spacing and scale; never judge UI at postage-stamp size. Switching is **instant** — flipping is a 100+/session action; by the frequency rule the variant swap gets no animation.

### Phase 5 — Verify and hand off

Run the project's typecheck, lint, tests, and build through `shell` where they apply. Check terminal results, not admission receipts. Load `emil/caudra/visual-check.md` with `skill` and follow it to inspect each variant. Without capture tooling, or in Plan mode, label visual findings **inferred from code** and give the user the exact interactions to check. Do not claim every variant renders or that the console is clean without evidence. Feel, smoothness, and real-device behaviour stay unverified until the user reports what they saw.

Then present the set. The choice belongs to the user:

| # | Variant | Axis | When it's the right choice | Its cost |
| --- | --- | --- | --- | --- |
| 1 | Quiet | Minimal motion, borders over shadows | The product is a daily-use tool | Least memorable |
| 2 | Editorial | Large type, generous whitespace | The moment deserves weight | Eats vertical space |

Close with where the picker is running and the keys to flip. In Plan mode, identify the saved draft instead of claiming the picker exists. Ask for the variant choice through `question`, with another round of exploration as an option. Put the recommended option first and end its label with "(Recommended)". Ground the recommendation in the product's personality and frequency of use, without preselecting a winner. If `question` is unavailable or has no answer channel, ask in plain text. A deferred, skipped, or dismissed answer stays unresolved.

**Completion criterion:** the table names each variant's tradeoff honestly, and every rendering, interaction, and console check has evidence or an explicit unverified status. In Plan mode, the variants are drafts, not working prototypes.

### Phase 6 — Promote on selection

After the user picks, ask through `question` whether to promote it, continue exploring, or keep the prototype only. Put your recommended answer first and end its label with "(Recommended)". If `question` is unavailable or has no answer channel, ask in plain text. Deferred, skipped, or dismissed answers are not approval. Promotion needs Build mode. In Plan mode, save the intended integration through the main-agent session-plan procedure and wait.

After approval in Build mode, integrate that variant where it belongs using the project's file layout, naming, and tokens. Re-run the applicable project checks. Clean up only what this skill created, per Hard Rule 5. If the user wants another round, keep the harness and run Phase 3 again, diverging *around* the direction they gravitated to.

## Invocation Variants

| Invocation | Behavior |
| --- | --- |
| `/emil:prototype <description>` | Scope, recon, up to 3 variants, picker, then choice and promotion through `question` |
| `/emil:prototype <description> x5` | Same, with up to 5 distinct variants |
| `/emil:prototype riff <variant>` | New round: keep the harness, generate a fresh set diverging around the named variant's direction |
| `/emil:prototype keep <variant>` | Confirm promotion through `question`, integrate in Build mode, then remove only what this skill created |
| `/emil:prototype keep <variant>, leave the picker` | Confirm promotion through `question`, integrate in Build mode, and keep the dev-only prototype |

## Tone

Sell each variant honestly — one line on when it wins, one on what it costs. Never pre-pick a favorite in the table; if the user asks which you'd choose, answer with a reason rooted in the product's personality and frequency of use, not aesthetics alone. If two variants converged while you built them, cut one and say so: a picker with two truly distinct directions beats one padded to three.
