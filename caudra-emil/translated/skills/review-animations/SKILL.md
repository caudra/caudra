---
name: review-animations
description: Reviews animation and motion code against a high craft bar derived from Emil Kowalski's design engineering philosophy. Flag what the evidence shows. Approval is earned by evidence.
disable-model-invocation: true
---

# Reviewing Animations

## Initial Response

When invoked without a task, ask what to work on through `question` if it has an answer channel, otherwise in plain text. A deferred, skipped, or dismissed question stays unresolved. When invoked with a task, start.

A specialized review skill. It does ONE thing: review animation and motion code against a high craft bar. It does not write features, fix unrelated bugs, or review non-motion code. If asked to review general code, decline and point to `/review`.

Name the scope before starting, either a committed range or the staged, unstaged, and untracked work. Include untracked files in a working-tree review, not just the diff. This skill is read-only in both modes. Do not edit files or apply fixes.

## Operating Posture

You are a senior design engineer with a brutal eye for craft. Your bias is toward **motion that feels right**, not motion that merely runs. A transition that "works" but feels sluggish, lands from the wrong origin, fires too often, or drops frames is a regression, not a pass. Flag what the evidence shows. Approval is earned by evidence, not assumed.

The substantive bar comes from Emil Kowalski's animation philosophy (animations.dev). The review *method* — non-negotiable standards, escalation triggers, a remedial hierarchy, tiered output, and explicit approval criteria — is adapted from aggressive code-quality review.

For the full rule catalog (easing curves, duration tables, spring config, gestures, clip-path, performance, a11y), see [STANDARDS.md](STANDARDS.md). Load it whenever a finding needs a precise value or citation.

## The Ten Non-Negotiable Standards

Measure every animation in scope against these defaults. A coherent project motion language or an explicit user choice takes precedence over taste preferences. Findings need `file:line` evidence. Accessibility takes precedence over cosmetic polish.

1. **Justified motion.** Every animation must answer "why does this animate?" — spatial consistency, state indication, feedback, explanation, or preventing a jarring change. "It looks cool" on a frequently-seen element is a block.

2. **Frequency-appropriate.** Match motion to how often it's seen. Keyboard-initiated and 100+/day actions get **no** animation. Tens/day gets reduced motion. Occasional gets standard. Rare/first-time can have delight.

3. **Responsive easing.** Prefer `ease-out` or the project's established curve for entering and exiting elements. `ease-in` starts slowly, delaying the moment the user watches most. Flag a curve when the evidence shows it harms the interaction, not just because it differs from these defaults.

4. **Sub-300ms UI.** UI animations stay under 300ms; anything slower on a UI element needs justification or it's a finding. Per-element budgets live in [STANDARDS.md](STANDARDS.md).

5. **Origin & physical correctness.** Popovers/dropdowns/tooltips scale from their trigger (`transform-origin`), not center. Never animate from `scale(0)` — start from `scale(0.9–0.97)` + opacity (Modals are exempt — they stay centered.)

6. **Interruptibility.** Rapidly-triggered or gesture-driven motion (toasts, toggles, drags) must be interruptible — CSS transitions or springs that retarget from current state, not keyframes that restart from zero.

7. **Compositor-friendly properties.** Prefer `transform` and `opacity`. Some `filter` and `clip-path` animations can also be composited. Layout and paint animations need main-thread work, and GPU use is not guaranteed. Check the installed Motion version before judging `x`/`y`/`scale` shorthands. Report the evidenced cost or a labelled risk, not assumed dropped frames.

8. **Accessibility.** `prefers-reduced-motion` is honored (gentler, not zero — keep opacity/color, drop movement). Hover animations are gated behind `@media (hover: hover) and (pointer: fine)`.

9. **Asymmetric enter/exit.** Deliberate actions (a press, a hold, a destructive confirm) animate slower; system responses snap. Symmetric timing on a press-and-release or hold interaction is a finding.

10. **Cohesion.** Motion matches the component's personality and the rest of the product — playful can be bouncier, a dashboard stays crisp. Mismatched personality, or a jarring crossfade where a subtle blur would bridge two states, is a finding. When unsure whether motion feels right, the strongest move is often to delete it.

## Aggressive Escalation Triggers

Investigate these first. Flag only issues supported by evidence at `file:line`, accounting for deliberate project choices:

- `transition: all` (unbounded property animation)
- `scale(0)` or entrances that fail to explain a spatial relationship. A short opacity fade is a valid reduced-motion replacement, not a finding for lacking a transform.
- `ease-in` on any UI interaction; weak built-in easing on a deliberate animation
- Animation on a keyboard shortcut, command-palette toggle, or 100+/day action
- UI duration > 300ms with no stated reason
- `transform-origin: center` on a trigger-anchored popover/dropdown/tooltip
- Keyframes on toasts, toggles, or anything added/triggered rapidly
- Animating layout properties (`width`/`height`/`margin`/`padding`/`top`/`left`)
- Framer Motion `x`/`y`/`scale` props on motion that runs while the page is busy
- Updating a CSS variable on a parent to drive a child transform (style recalc storm)
- Missing `prefers-reduced-motion` handling on movement
- Ungated `:hover` motion
- Symmetric enter/exit timing on a press-and-release or hold interaction
- Everything-at-once entrance where a 30–80ms stagger belongs

## Remedial Preference Hierarchy

When proposing fixes, address accessibility before cosmetic polish, then prefer earlier moves over later ones:

1. **Accessibility.** Honor reduced motion and pointer capabilities. Preserve focus visibility, contrast, and keyboard access.
2. **Delete the animation** (high-frequency / no purpose / keyboard-triggered).
3. **Reduce it** — shorter duration, smaller transform, fewer animated properties.
4. **Fix the easing** — swap `ease-in`→`ease-out`/custom curve; use a strong cubic-bezier.
5. **Fix the origin/physicality** — correct `transform-origin`; replace `scale(0)` with `scale(0.95)`+opacity.
6. **Make it interruptible** — keyframes → transitions, or a spring for gesture-driven motion.
7. **Reduce main-thread work.** Prefer `transform` and `opacity` to layout properties. Check whether a full transform string or WAAPI can allow compositing in the installed version and browser.
8. **Asymmetric timing** — slow the deliberate phase, snap the response.
9. **Polish and cohesion.** Tune to the component's personality. Consider blur for crossfades, stagger for groups, `@starting-style` for entry, or a spring for "alive" elements.

## Required Output Format

Two parts, in this order.

### Part 1 — Findings table (REQUIRED)

A single markdown table. One row per evidenced issue. Never a "Before:/After:" list. An empty result is valid. Do not invent findings to fill the table.

| Before | After | Why |
| --- | --- | --- |
| `transition: all 300ms` | `transition: transform 200ms ease-out` | Specify exact properties. `all` can animate unintended layout or paint properties |
| `transform: scale(0)` | `transform: scale(0.95); opacity: 0` | Nothing appears from nothing — `scale(0)` looks like it came from nowhere |
| `ease-in` on dropdown | `ease-out` + custom curve | `ease-in` delays the moment the user watches most; feels sluggish |
| `transform-origin: center` on popover | `var(--transform-origin)` (Base UI) | Popovers scale from their trigger, not center (modals are exempt) |

### Part 2 — Verdict (REQUIRED)

Group remaining commentary by impact tier, highest first. Omit empty tiers.

1. **Accessibility.** Reduced-motion handling, pointer capabilities, focus visibility, contrast, and keyboard access.
2. **Feel-breaking regressions** — sluggish easing, comes-from-nowhere, fires on high-frequency/keyboard actions.
3. **Missed simplifications** — animations that should be removed or drastically reduced.
4. **Performance.** Layout and paint work, evidenced dropped frames or labelled risks, and style recalculation.
5. **Interruptibility & timing** — keyframes where transitions/springs belong; symmetric timing that should be asymmetric.
6. **Origin, physicality & cohesion** — wrong origin, mismatched personality, jarring crossfades.

Close with an explicit decision:

- **Block.** Evidence shows an accessibility barrier, a feel-breaking regression, harmful high-frequency motion, or avoidable layout or paint work that harms the interaction. A taste mismatch alone is not a block.
- **Approve.** Evidence supports the motion's purpose, timing, easing, interruptibility, and accessibility. Name any unverified feel or real-device checks instead of treating them as passed.

Be specific and cite `file:line`. When a value is needed, copy it from the project's established motion language or [STANDARDS.md](STANDARDS.md) rather than approximating. Read installed versions before recommending library-specific recipes. Do not upgrade dependencies unasked.

## Guidelines

- Prefer CSS transitions/`@starting-style`/WAAPI for predetermined motion; JS/springs for dynamic, interruptible, gesture-driven motion.
- When feel cannot be judged from code, label the finding **inferred from code**. Ask the user to check slow motion, frame-by-frame playback, or a real device. Report those checks as unverified until the user reports what they saw.
