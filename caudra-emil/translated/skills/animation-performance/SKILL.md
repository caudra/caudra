---
name: animation-performance
description: Keep web animations inside the frame budget — animate composite-only properties, keep motion off the main thread when the page is busy, and find the cause when an animation drops frames. Covers the layout/paint/composite pipeline, cheaper property swaps, CSS vs WAAPI vs requestAnimationFrame, Framer Motion's hardware-accelerated path, React re-rendering per frame, inherited CSS variable recalculation, blur cost, and when `will-change` actually helps. Use when an animation is janky, choppy, stutters, or drops frames; when choosing which property to animate; when deciding between CSS and JS animation on performance grounds; or when reaching for `will-change` or GPU layers. For choosing easing, duration, or whether to animate at all use animate; for reviewing motion as a whole use review-animations.
---

# Animation Performance

## Initial Response

Invoked without a task, ask what animation to diagnose through `question`, or in plain text if it is unavailable or has no answer channel. A deferred, skipped, or dismissed answer stays unresolved. Invoked with a task, start.

A diagnostic skill. It asks whether each frame finishes in time. For easing or duration, load `emil/animate` with the `skill` tool. For judging whether motion should exist, suggest `/emil:review-animations`. For React Native's threads, load `emil/animate-expo`.

Report first. The diagnosis is read-only; every change below is a proposed fix until the user asks to apply it. In Plan mode, the main agent reads the session plan with `plan`, preserves unrelated sections, and saves the complete revised content with the intended fixes, then waits for Build mode. Tasks return drafts without writing the plan. Saving neither approves the work nor changes mode.

## Why This Matters

Imagine [Sonner](https://sonner.emilkowal.ski/) running at 30fps. Same easing, same durations, same code. Nobody would have cared. Performance isn't a polish pass applied after the motion is good; it's what lets good motion register at all.

## Operating Posture

You are a design engineer who has shipped drawers, toasts, and dashboards to real devices and has had each of the traps below bite in production. You know the developer's MacBook is the least representative device the animation will ever run on.

Two failure modes, and the first is worse:

1. **"It's smooth on my machine."** A fast laptop hides almost every problem in this skill. The bar is a mid-range phone, and on a 120Hz screen the budget is half what you think.
2. **Sprinkling `will-change` and calling it fixed.** It's the last step, not the first. Most jank is the wrong property or a busy main thread, and `will-change` fixes neither.

## Hard Rules

1. **`transform` and `opacity` by default.** Anything else needs a reason. If a cheaper property produces the same visual, use it, even when the expensive one "seems fine".
2. **Name the cause before changing code.** Every fix you propose names which of the causes below it addresses. "Added `will-change`, seems smoother" is not a diagnosis.
3. **Never conclude "it's fine" from a fast machine.** Say which findings you verified from code and which need a recording on a real device.
4. **Never `transition: all`.** List the exact properties. `all` animates whatever changes next, including layout properties nobody intended to animate.
5. **Don't trade correctness for a best-effort optimization.** Hardware acceleration is something the browser may grant. Which properties you animate is something you control. Spend effort on the second.

## The Symptom Table

Start here. Match what the user is seeing, then read the matching section.

| Problem | Solution |
| --- | --- |
| Animation stutters | Animate `transform`/`opacity`, not `width`/`height`/`top`/`margin` |
| Shadow animation is choppy | `filter: drop-shadow()` |
| Jank only during navigation or loading | Consider CSS or WAAPI on compositor-eligible properties |
| Framer Motion `x`/`y` drops frames | Animate the full `transform` string |
| Hand-rolled animation drops frames in React | Write to `ref.current.style`, not state |
| Drag gets laggier as the list grows | Stop animating an inherited CSS variable; set `transform` directly |
| Blur transition lags (especially Safari) | Keep animated `blur()` under 20px |
| Unexpected properties animate | Replace `transition: all` with explicit properties |
| Element shifts 1px when motion starts or ends | `will-change: transform`, only once you see it |

## The Frame Budget

60fps is what the brain reads as fluid motion and matches most refresh rates. It gives the browser **16.7ms** (1000/60) per frame. On a 120Hz screen, now most phones and many laptops, it's **8ms**. Design against the smaller number.

For scale: a layout recalculation on a complex page can take over 100ms. That isn't one missed frame, it's a dozen.

Two things decide whether you stay inside the budget:

1. **What you animate**: how much work each frame costs.
2. **Where it runs**: whether that work can be blocked by everything else the page is doing.

## What You Animate

To show a visual change, the browser runs up to three steps:

- **Layout**: calculate the size and position of every element.
- **Paint**: draw the page into layers.
- **Composite**: put those layers on screen.

Properties enter this pipeline at different points. Cost depends on the page, browser, and device:

| Tier | Properties | Cost |
| --- | --- | --- |
| Can be composite only | `transform`, `opacity`; some `filter` and `clip-path` animations | Can avoid per-frame layout and paint. Promotion depends on browser and effect. |
| Usually paint + composite | `box-shadow`, `border-radius`, `color`, `background-color` | No re-measuring, but typically a redraw every frame. |
| Layout + paint + composite | `width`, `height`, `padding`, `margin`, `top`, `left`, `border-width` | Most expensive. One growing element can move everything around it. |

`transform` and `opacity` change how an element looks, not where it sits in the document flow, so nothing needs re-measuring. Anything else in the composite tier needs testing across browsers and devices; support is uneven.

**Swapping down a tier:**

| Instead of | Animate |
| --- | --- |
| `width` / `height` / `padding` to grow or shrink | `scale()` |
| `top` / `left` / `margin` to move | `translate()` (percentages are relative to the element's own size) |
| `display` / `visibility` swaps | `opacity` |
| `box-shadow` | `filter: drop-shadow(…)` |

The last one is the one people miss. An animated shadow looks harmless but repaints the element every frame.

Two things follow:

- **Layout animations don't always drop frames.** An absolutely positioned element, or one with few children, may animate `padding` without visible cost. A `scale()` animation can be cheaper, but also scales children and does not reflow surrounding content. Propose it only when it preserves the intended result; neither approach guarantees smoothness.
- **Layout properties also move the wrong things.** Animating a sidebar's `margin-left` pushes the content beside it on every frame; `translateX` doesn't. For a dashboard next to an opening sidebar, that's the whole page re-laid-out 60 times a second.

## Where It Runs

**DOM animation driven by `requestAnimationFrame` runs on the main thread.** It competes with other main-thread work. When that thread is busy, the animation's code can't run and the frame is late. JavaScript can also run in workers, which cannot update the DOM directly.

| Driver | Thread |
| --- | --- |
| DOM updates through JS + `requestAnimationFrame` | Main thread |
| Web Animations API (`element.animate()`, library paths that use it) | Compositor for eligible properties and effects |
| CSS transitions and keyframes | Compositor for eligible properties and effects |

Compositor animation can advance without a main-thread callback every frame. GPU acceleration is a separate browser decision, and neither guarantees smoothness. A JS library can hand animation to WAAPI; a DOM-updating rAF loop still needs the main thread. Animating layout or paint properties needs main-thread work even through CSS or WAAPI.

Prefer `transform` and `opacity`. Some `filter` and `clip-path` animations can also be composited, depending on the browser and effect. Verify the actual path rather than assuming GPU use.

**The pattern to watch for is motion that runs while the page is doing heavy work**: page transitions, tab indicators during navigation, anything that fires alongside data fetching or hydration. Vercel's dashboard animated its active tab indicator with a Framer Motion shared layout animation. The browser was loading the next page at exactly that moment, so it dropped frames. Moving it to a CSS animation fixed it.

Read the installed Motion version before choosing a fix. A full transform string can let Motion use a compositor-eligible animation path:

```jsx
<motion.div animate={{ x: 100 }} />                          // can stay on the main thread
<motion.div animate={{ transform: "translateX(100px)" }} />  // can run on the compositor
```

The `x`, `y`, `scale`, and `rotate` shorthands can use main-thread updates or CSS variables, depending on the version and animation path. Keep them by default and consider the string form when profiling identifies a busy-main-thread problem. Never upgrade the dependency unasked.

**Acceleration is progressive enhancement.** Browsers silently decline it under conditions that change between versions; Chrome, for example, historically didn't accelerate percentage-based transforms. That's not a reason to stop using `translateY(100%)`. Percentages are the right call for elements of unknown size (it's how Sonner and Vaul position toasts and drawers), and correctness beats a best-effort optimization.

## React: Don't Animate Through State

Animation libraries animate outside React's render cycle, so they're fine. The trap is a hand-rolled animation that calls `setState` every frame: the component re-renders every 16.7ms and the render alone blows the budget.

```jsx
// Drops frames: one re-render per frame
setY(nextY);

// Smooth: no re-render
ref.current.style.transform = `translateY(${nextY}px)`;
// or with a motion value: y.set(nextY)
```

Look for state updates inside `requestAnimationFrame`, `pointermove`, `scroll`, or drag handlers.

## Two Traps That Look Correct

Both of these use `transform` and still drop frames.

**Inherited CSS variables.** Vaul's drag got laggy once a drawer held more than ~20 items, with no re-renders involved. The drag distance was written to a CSS variable that `translateY()` read:

```js
// Recalculates styles for every descendant, every frame
const style = { "--swipe-amount": `${draggedDistance}px` };

// Only touches this element
const style = { transform: `translateY(${draggedDistance}px)` };
```

Unregistered CSS custom properties inherit, so changing one on a parent can trigger style recalculation in descendants. Suspect this whenever a `transform` animation degrades as content grows. Check the installed Motion version before attributing the same cost to its shorthands.

**Blur.** `filter: blur()` gets expensive fast, especially in Safari. Keep animated blur under 20px; 2–5px is enough to soften a crossfade.

## `will-change`

Layer promotion can change rasterization and produce a visible shift. `will-change` hints that the browser should prepare for a change; it does not guarantee layer promotion or GPU use:

```css
.drawer {
  will-change: transform;
}
```

**Propose it only after a visible shift or measured dropped frames.** Browsers often prepare layers for `transform` or `opacity` without a hint, and extra layers cost memory. Put it on the element that animates, never on `*` or large containers. Confirm that it helps before keeping it.

It may help with an expensive paint, such as a blurred or filtered element. Test the hint and keep the layer small; a small layer is cheaper to composite than a full-screen one.

## Should You Avoid JS Animation?

No. Main-thread animation only suffers when the page is busy during it, which is rare, and now you know how to spot it.

Framer Motion gives you things CSS can't: real springs, shared layout animations, exit animations for unmounted components, momentum gestures. The question is whether you need those, not whether JS animation is allowed.

**The working split:** CSS for simple motion and anything that plays while the page is busy; Framer Motion for complex, interruptible, gesture-driven motion.

## Workflow

### Phase 1: Find what animates

Read the package manifest and installed versions. Use `source_code_context` and `file_index` to locate animation definitions, `source_code_refs` for their uses, and `file_grep` for CSS and other text searches. Graph counts are lower bounds. List every animation in scope with its driver and properties:

| Element | Driver | Properties | Plays during |
| --- | --- | --- | --- |
| Tab indicator | `motion/react` `layoutId` | layout | route change |
| Drawer | CSS transition | `transform` | user drag |
| Card hover | CSS transition | `all` | hover |

Expand `transition: all` and Motion shorthands into what they actually animate.

### Phase 2: Diagnose

Work down this list. It's ordered by how often each one is the cause:

1. **Which properties animate?** Anything other than `transform`/`opacity` is the first suspect: layout properties first, then `box-shadow` and `border-radius`, which don't look like layout and are easy to miss.
2. **Does the jank coincide with the page being busy?** Navigation, fetching, hydration, a long task. Consider CSS or WAAPI if the properties and effect can be composited.
3. **Is React re-rendering per frame?** State updates in rAF, pointer, or scroll handlers.
4. **Does it get worse as content grows?** An inherited CSS variable, or too many nodes animating at once.
5. **Is a CSS variable animated at all?** Directly, or through Motion's `x`/`y`/`scale`.
6. **Any animated `blur()` above 20px?**
7. **Still shifting or stuttering?** Propose testing `will-change: transform` on the animating element.

Ask the user to confirm with a recording. Have them use the DevTools Performance panel for dropped frames and long tasks, the Animations panel to slow motion down, and remote debugging on a real mid-range phone. CPU throttling on a desktop models a slow CPU, not a weak GPU or a small memory budget. Report these checks as unverified until the user answers. Load `emil/caudra/visual-check.md` with the `skill` tool for rendered checks; a still image cannot prove smoothness.

### Phase 3: Report

For each finding:

```
**[Element]**: `file.tsx:42`
Cause: [which of the causes above]
Before: `transition: all 200ms` (animates `padding`, triggers layout)
Proposed: `transition: transform 200ms` with `scale(0.97)`
Evidence: inferred from code / user-reported recording
Runtime check: unverified until the user reports it
```

Order findings by cost: layout before paint, busy-main-thread before idle. Then stop. Apply fixes when asked.

An empty findings list is valid. If the user requests fixes in Build mode, run the project's typecheck, lint, tests, and build through `shell`. Report terminal results separately from unverified smoothness and device checks.
