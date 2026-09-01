# Docs design system

**Routing editorial.** The docs share Caudra's terminal-native identity. Text and
navigation carry the interface. The decision-aperture mark places a midnight C
around one coral route that continues as action.

## Principles

1. Fix measure, leading, and hierarchy before adding components.
2. Signal color identifies a link, active route, focus state, or selected action.
3. Prefer useful density over marketing whitespace.
4. Keep geometry square and borders visible.
5. Make no external requests. Fonts and assets are self-hosted.

## Type

- Body: system sans stack, 16px, line-height 1.6, and a 680px content measure.
- Headings, navigation, labels, tables, and code: self-hosted JetBrains Mono.
- Use weight and space for hierarchy. Heading size stays restrained.
- Labels are uppercase monospace at 0.64 to 0.7rem with wide tracking.

## Color

Midnight navy and warm mineral white form the base. Vermilion marks active routing.
Components consume variables from `base.html` rather than defining local themes.

| Role | Light | Dark |
| --- | --- | --- |
| Surface | `#f2ebdd` | `#09254d` |
| Ink | `#09254d` | `#f2ebdd` |
| Muted | `#696861` | `#a7a69e` |
| Signal | `#b52f20` | `#ff7662` |

Vermilion appears on links, active navigation, focus rings, search marks, and
selected-route details. Tips and warnings keep distinct semantic colors.

## Code blocks

Code uses a midnight pane in both themes. The `caudra` syntax theme lives in
`extra/caudra.json` and requires Zola 0.22 or newer. Vermilion is reserved for
functions and commands. Strings, constants, types, and keywords use restrained
high-contrast hues so longer examples remain readable.

## Components

- Flat surfaces, 1px borders, and a 2px active edge form the component vocabulary.
- Inline code and keycaps may use a 2px radius. Architectural components stay square.
- The docs index uses dense list rows with a fixed title column.
- Tables use uppercase monospace headers.
- Code blocks reveal a copy button on hover or keyboard focus.
- Heading anchors and per-page token estimates are the two idiosyncratic details.

## Avoid

- display serifs and rounded marketing sans faces
- purple, lime, or pastel identity colors
- gradients, glass effects, star fields, glows, and decorative washes
- mascots, food imagery, brains, neural networks, and generic AI symbols
- hover lifts, pill badges, rounded cards, and fake terminal title bars
- shadows outside the search modal and mobile navigation

## Performance

- No external requests.
- Preload only the normal mono face. Load italic when needed.
- Animate opacity or transforms and honor reduced motion.
- Lazy-load `search.json` and prefetch internal pages on hover.
- Keep the docs CSS and JavaScript inline to preserve one-request page loads.

## Verification

Run `./build.sh` from `site` for the complete output, search index, Markdown
mirrors, and token counts. Run `zola build` from `site/docs` for a faster docs-only
check. Inspect light and dark themes at 390px and 1600px.
