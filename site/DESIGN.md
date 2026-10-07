# Website design system

**A coding agent that turns smart context into effective action.** This is the
homepage headline. Its short form, “Context into effective action”, serves page
titles and metadata. Caudra combines an agent and the surrounding development
work in one native terminal application, for interactive work and unattended
runs.

Day/night copy does not claim shipped scheduled automations or background work
that survives closing its owning session. The homepage shows work in use. The
docs prioritize reading, navigation, and finding an answer.

## Principles

1. Fix measure, leading, and hierarchy before adding components.
2. Signal color identifies a link, active route, focus state, or selected action.
3. Give product demonstrations room on the homepage. Keep docs usefully dense.
4. Keep surfaces flat, geometry deliberate, and borders visible.
5. Self-host fonts, artwork, and recordings. No third-party runtime requests.
6. Show actual product behavior. Do not simulate evidence or invent measurements.

## Type

- Body, headings, and navigation: self-hosted Space Grotesk, variable weight 300–700.
- Commands, code, technical labels, and table headers: self-hosted JetBrains Mono.
- Character-cell diagrams: JetBrains Mono as well. Its subset keeps the font's arrows, box drawing, block elements, and geometric shapes. Draw only with glyphs it has, because a fallback glyph of another width breaks the grid.
- Docs body: 16px, line-height 1.6, and a 680px content measure. Heading sizes stay restrained.
- Homepage: oversized, closely tracked display type and shorter editorial measures. Do not enlarge the docs layout into a landing page.
- Use weight and space for hierarchy. Technical labels may use uppercase monospace with wide tracking.

Space Grotesk comes unmodified from the [Florian Karsten repository](https://github.com/floriankarsten/space-grotesk/)
at revision `03507d024a01282884232081fc6011c09ff4e849`, source
`fonts/woff2/SpaceGrotesk[wght].woff2`. The SIL Open Font License 1.1 and copyright
notice are in `public/fonts/space-grotesk-OFL.txt`. The full variable WOFF2 is
49,256 bytes and includes the upstream Latin language coverage.

JetBrains Mono 2.211 comes from the [Google Fonts repository](https://github.com/google/fonts/tree/main/ofl/jetbrainsmono)
at revision `6e4b84c976cadb3c49a40fd9a1c203e4f7fcf2da`, sources `ofl/jetbrainsmono/JetBrainsMono[wght].ttf`
with its weight axis limited to 400–800 and `JetBrainsMono-Italic[wght].ttf` instanced at 400. Both are
subset to Google Fonts' Latin range plus every glyph the font has from U+2070 to U+2BFF, keeping the
`calt`, `ccmp`, `frac`, `locl`, and `mark` features. The WOFF2 files are 37,404 and 27,292 bytes. The SIL
Open Font License 1.1 and copyright notice are in `public/fonts/jetbrains-mono-OFL.txt`.

## Color

Graphite and clean off-white form the base. Cobalt identifies action and control.
Components consume shared brand variables from `src/styles/`. The dark homepage
opening contrasts with light editorial sections. Docs retain both themes.

| Role | Light | Dark |
| --- | --- | --- |
| Surface | `#f5f5f2` | `#14151a` |
| Ink | `#14151a` | `#f5f5f2` |
| Panel | `#e9e9e6` | `#1b1d24` |
| Muted | `#646773` | `#a5a7b0` |
| Signal | `#315bff` | `#8ca6ff` |
| Border | `#c9cbd2` | `#34363f` |
| Scrollbar thumb | `#858896` | `#686c7c` |

Homepage sections set one tone: `tone-light`, `tone-mist`, `tone-dark`, or
`tone-signal`. A tone defines `--surface`, `--raised`, `--text`, `--text-muted`,
`--line`, `--line-strong`, and `--accent`, and components use only those, so any
component works on any tone. `home.css` holds no literal colors. New colors go
into `brand.css` as named tokens first.

Cobalt appears on links, active navigation, focus rings, search marks, and selected
details. Use the brighter signal on graphite, not the light-theme action color.
Tips, warnings, and syntax keep distinct semantic colors. Status needs text or
an icon as well as color. Main scrollbars stay native width, sidebar scrollbars
stay thin, and forced-colors mode delegates colors to the system.

## Mark and assets

The mark is an open C loop and an offset route, drawn as simple SVG paths. An
off-white loop and bright cobalt route sit on a graphite square so the mark works
on either page theme and at favicon sizes. The lowercase wordmark uses outlined
Space Grotesk at weight 600, with light/dark text chosen by the SVG color scheme.

`public/caudra-mark.svg` is the geometry source. PNG icons use the same paths,
rendered at four times their target size with CairoSVG and downsampled with
Pillow's Lanczos filter. The ICO contains 16, 32, 48, 64, 128, and 256px sizes.
Touch and app icons are 180, 192, and 512px. The social image is 1200×630.

`public/social-card.svg` is the current social artwork. Its text and the wordmark
are SVG outlines from the licensed font using fontTools, so rendering does not
depend on installed system fonts. Render the social SVG directly with CairoSVG
to refresh its PNG. The artwork carries the hero eyebrow “Open source, for your
terminal” and the headline “A coding agent that turns smart context into
effective action.” `SOCIAL_CARD_ALT` in `src/data/home.ts` holds the alt text
for both lines, so change it together with the artwork.
Do not reuse the historical aperture/coral artwork kept outside `public/`.

## Code blocks

Code uses a graphite panel in both docs themes through Starlight and Expressive
Code. Bright cobalt identifies functions and commands. Strings, constants, types,
and keywords use restrained high-contrast hues so longer examples remain readable.
`src/styles/caudra-theme.json` is website-only. Leave TUI themes and the colors
inside product recordings unchanged.

## Components

- Flat surfaces, 1px borders, and a 2px active edge form the component vocabulary.
- Inline code and keycaps may use a 2px radius. Architectural components stay square.
- The docs index uses dense list rows with a fixed title column.
- Tables use uppercase monospace headers.
- Code blocks reveal a copy button on hover or keyboard focus.
- Heading anchors and per-page token estimates are the two idiosyncratic details.

## Avoid

- display serifs and full-page monospace marketing typography
- competing identity palettes
- gradients, glass effects, star fields, glows, and decorative washes, apart from the single cobalt light behind the hero clip
- mascots, food imagery, brains, neural networks, and generic AI symbols
- hover lifts, pill badges, rounded marketing cards, and fake terminal title bars
- shadows outside the search modal and mobile navigation

## Product evidence and motion

Use sanitized real recordings and useful still frames. Missing footage does not
justify a fabricated terminal, an unapproved screenshot, or a nonfunctional play
button. Until a reviewed capture arrives, its slot shows a small static diagram
captioned “Illustration”. Diagrams show order or structure, never invented
output, timing, or interface chrome. `RECORDINGS.md` covers the shot list,
privacy checks, posters, and the manifest entry that replaces a diagram.

Playback is explicit and lazy-loaded, never a prerequisite for installation or
reading. Preserve original timing and label edits. Provide summaries, keyboard
controls, legible mobile presentation, and reduced-motion behavior. Do not imply
that a recording duration measures performance.

The proof strip shows the maintainer's own measurements from daily use. Each
figure stays attributed where it appears and links to the method note. They are
observations, not general benchmarks.

## Scroll motion

The homepage gains depth from CSS scroll-driven animations in
`src/styles/motion.css`. There is no script and no motion library, and the page
never takes over scrolling.

- Every rule sits inside `screen`, `prefers-reduced-motion: no-preference`, and
  `@supports (animation-timeline: view())`. Other visitors see the static page.
- Animate `transform`, `translate`, `scale`, `rotate`, `opacity`, or
  `clip-path`. The name loop steps may also animate `color`, from the muted text
  color to the full one.
- Name keyframes with the `depth-` prefix. The browser test checks every
  `depth-` animation for its timeline and properties.
- Below 761px, keep the entry reveals, the progress line, and the static hero
  light. Layered parallax, the tilt, and the wipes start at 761px. The name
  loop pins only in windows at least 1101px wide and taller than 700px.
- Text stays legible at every scroll position. Reveals start at the viewport
  edge and finish while the item is still entering.
- Decorative layers are `aria-hidden`, ignore the pointer, sit behind content,
  and disappear in forced colors.
- Clip decorative overflow with `overflow: clip`. An ancestor with `overflow:
  hidden` or `auto` becomes the scroll container that `view()` measures, which
  freezes the animation.

## Performance

- No external requests.
- Preload Space Grotesk and the normal mono face. Load mono italic when needed.
- Animate opacity or transforms and honor reduced motion.
- Use Starlight's Pagefind search rather than a separate search index or service.
- Let Astro bundle styles and scripts. Keep site-specific client code small.

## Verification

From `site`, run `bun run check`, `bun run test`, `bun run build`,
`bun run test:output`, and `bun run test:browser`. Production output in `dist/`
includes docs, search, Markdown mirrors, and LLM exports. Coordinate builds when
multiple agents share the worktree.

Inspect homepage and both docs themes at 390px, 1600px, intermediate widths, and
text zoom. Check focus, selection, search, syntax, native-width scrollbars,
forced colors, reduced motion, and no-JavaScript fallbacks. Check font and image
requests remain local. Verify browser search against the production build.
