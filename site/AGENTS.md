# Website and docs

## Website workflow

- This directory is the standalone Astro + Starlight application in the Rust monorepo. Keep package metadata, dependencies, tests, assets, and build output here.
- Use Bun 1.4.2 and the committed `bun.lock`. Install with `bun install --frozen-lockfile`. Do not add another package-manager lockfile.
- Run `bun run dev`, `bun run check`, `bun run test`, and `bun run build` from this directory. Run `bun run test:output` and `bun run test:browser` against the production build. Root `just site-*` recipes delegate here. Production output is `dist/`.
- Use Starlight for standard docs navigation, search, themes, and code blocks. Keep overrides small. Brand styles belong in `src/styles/`; consult `DESIGN.md`.
- Put processed assets in `src/assets/` and published passthrough files in `public/`. Do not publish source artwork by accident or duplicate root installer sources.

## Docs style

Audience: competent devs. Every line earns its place. No hand-holding, no fluff.

## Voice

- Warm, simple, concise; easy for non-native English readers.
- No em-dashes, emojis, contractions, or hedging ("it's worth noting", "generally speaking").
- Plain words over clever idioms: "sends data only to the endpoint you configure", not "never phones home".
- State facts and why they matter; never perform emphasis.
- Vary sentence length; uniform 18-24 word cadence reads machine-made.
- Use Mermaid when a diagram communicates structure more clearly than prose.

## Banned AI mannerisms

- Rule-of-three lists for rhythm instead of content.
- "not X, but Y" / "not just X, it's Y" flips; use "rather than" or state the fact once.
- Balanced antithesis ("everything X, nothing Y").
- Emphatic negation tails ("nowhere else", "nothing more").
- Punchy fragment openers ("Nothing blocks.") and tidy end-of-paragraph summations.
- Parallel negation pairs ("does not X and does not Y").
- Stock vocab: delve, robust, seamless, leverage, landscape, moreover, furthermore.
- Semicolons. Split the sentence or join with "and".

Fine when genuine: anaphora in scannable checklists ("No prompt text / No model output"), contrasts carrying real information ("`session.id` is on by default, `app.version` is not").

## Structure

- [Diátaxis](https://diataxis.fr/): guides for goals, reference for lookup, concepts for understanding.
- One canonical home per topic; link instead of duplicating.
- Generated pages (tools, providers, configuration, lua-api, plugins, keybindings, commands) come from `caudra-docgen`: edit the source, run `just gen-docs`, never edit output by hand.
- Hand-written pages can hold generated regions between `<!-- caudra-docgen:NAME -->` and `<!-- /caudra-docgen:NAME -->`. Write around the markers, never between them. The `*.example.toml` files in `public/docs/` are generated too.

## In the binary

- Every page under `src/content/docs/` is compiled into Caudra. The agent reads it through the builtin `caudra-docs` skill, and users read it in the `/docs` modal. Rust reads the sources directly and must not require Bun or a website build.
- Pages use YAML `title` and `description` metadata. Do not repeat the title as an H1 in the body. Rust supplies it for terminal/model rendering, and Starlight supplies it for the website.
- `src/data/docs-navigation.json` owns navigation groups and ordering for both consumers. List each non-index page exactly once. `index.md` is the plain-Markdown overview, not an HTML navigation manifest.
- Keep shared content ordinary Markdown. Do not use MDX imports, Astro components, or Starlight-only directives in bundled pages. Site-only presentation belongs in components.
- Tests resolve every `/docs/<page>/#<anchor>` and `#<anchor>` link against the page headings, so renaming a heading means updating its links.
- Search corrects a misspelt word only while that word appears nowhere in the docs. Never write a misspelling as an example.
- The TUI renders no inline HTML. It keeps the text inside a tag, shows a `badge` span as inline code and `<br>` as a space, and prints blockquotes as plain lines.
- An HTML comment on lines of its own, such as a `caudra-docgen` region marker, is dropped before the modal or the model sees the page. A comment inside a paragraph is not dropped, so never put one there.

## Product identity

- Position Caudra as a terminal coding agent that turns context into effective action.
- State that Caudra is an independent fork when project provenance is relevant.
- Use `Caudra` for the product and `caudra` for commands, paths, packages, APIs, and the lowercase wordmark.
- Use only `https://caudra.ai` for the site, docs, and installer origin.
- Use `github.com/caudra/caudra` for source and `github.com/caudra/config` for the example config.
- Telemetry names use the `caudra.*` namespace.

## Visual identity

- Use the lowercase wordmark and decision-aperture mark. Its midnight shell forms a C around one coral route.
- Base surfaces are midnight navy and warm mineral white. Vermilion marks a selected route or active state.
- Use precise curves, high contrast, and functional decoration.
- Do not use mascots, food imagery, literal brains, neural networks, gradients, glass effects, or rounded marketing cards.
