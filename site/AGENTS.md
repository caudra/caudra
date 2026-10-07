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
- Generated pages (tools, providers, configuration, reference-configs, lua-api, plugins, keybindings, commands) come from `caudra-docgen`: edit the source, run `just gen-docs`, never edit output by hand.
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

- The homepage headline is “A coding agent that turns smart context into effective action.” The short brand line “Context into effective action” serves titles and metadata. Do not show both side by side.
- Lead with what Caudra does: effective action from coordinated models, tools, and subagents, adapted to each outcome and kept under user control. Prefer concrete actions and outcomes over claims about intelligence. Never imply the agent always knows the right answer.
- Position Caudra for interactive work and unattended runs: agent work, native tools, a workbench, and resumable sessions in one terminal application.
- Day/night messaging does not claim shipped scheduled automations or imply that background work survives closing its owning session.
- Caudra is pronounced KAW-druh, IPA /ˈkɔːdrə/, and is named after the caudate nucleus, part of the brain circuits that connect evidence and goals to action. Brain references belong only in the name story (homepage `#name`, README “Why the name”), which cites Grahn, Parkinson and Owen (2008), Lau and Glimcher (2007), and Doi et al. (2020). The analogy is limited: never imply consciousness, biological equivalence, or goals of its own.
- State that Caudra is an independent fork when project provenance is relevant.
- Use `Caudra` for the product and `caudra` for commands, paths, packages, APIs, and the lowercase wordmark.
- Use only `https://caudra.ai` for the site, docs, and installer origin. An unrelated apparel business owns `caudra.com`, so never imply any connection to it.
- Use `github.com/caudra/caudra` for source and `github.com/caudra/config` for the example config.
- Telemetry names use the `caudra.*` namespace.

## Homepage claims

- Homepage copy lives in `src/data/home.ts`, and components render it. Every capability links to its docs section. Document a shipped behavior in the canonical docs before the homepage claims it.
- Founder figures are the maintainer's own measurements from daily use. Show them only with attribution in the same section and the method note at `#method`. Never present them as benchmarks, and do not name the other agent in the storage comparison. Output tests enforce the attribution.
- Credit RTK, ripwire, Plannotator, and Herdr as inspiration, each with its own link. Credit Grok Build where durable workflows appear, never in the founder section, where a named coding agent would read as the subject of the storage comparison. Do not imply affiliation or endorsement. No comparison tables or superiority claims about named products.
- Copy in `home.ts` may carry Markdown links, `[text](href)`, which `Inline.astro` renders.
- Wherever Claude subscription sign-in appears, show that it is experimental and that Anthropic's terms limit Pro and Max subscriptions to official clients. Present ChatGPT, Copilot, and xAI sign-in plainly.
- Mark every experimental capability as experimental. Never present one as a default.
- Describe file change records, not snapshots. Built-in web search is Exa. Say "offline with a local model", and name the models.dev catalog refresh and Exa searches where network use matters.
- Credit Workcell, with a link to its repository, where the file, shell, web, code, and Python tools appear. Tool claims follow the generated Tools page: searches report what they withheld, directory searches skip credential files, `webfetch` refuses private addresses, checks every redirect, decodes declared character sets, and names the limit when it cuts a page, and a failing command keeps its first and last lines. Where a story or the README describes PDF attachments, name the transports that send them and say that other models receive the text.
- Never call the Python worker a sandbox. Leave out "personalized PageRank", git signals, ripwire benchmark numbers, and Monty startup times, and do not claim that filtering keeps errors.

## Visual identity

- Use graphite `#14151a`, off-white `#f5f5f2`, and cobalt `#315bff`. On dark surfaces, use `#8ca6ff` for links, focus, and selected details. Shared tokens live in `src/styles/brand.css`.
- Use self-hosted Space Grotesk for body and display type, and JetBrains Mono for commands, code, and technical labels. Keep font licenses alongside assets. No runtime font CDN.
- Lead the homepage with real product workflows and generous typography. Keep docs focused on reading and standard Starlight navigation, search, and themes.
- Preserve native-width main scrollbars, thin docs sidebars, visible keyboard focus, reduced-motion support, and system-controlled forced colors.
- Keep original TUI colors in product captures. The website palette does not change the terminal themes.
- Do not use fake sessions, dummy playback controls, invented metrics, stock AI art, gradients, glass effects, glow fields, or fake terminal title bars. The one exception is the single cobalt light behind the hero clip. Missing recordings remain an explicit asset dependency.
- Record, check, and publish terminal footage as `RECORDINGS.md` describes. Until a reviewed recording exists, its slot shows a small static diagram captioned "Illustration". Diagrams show order or structure, never invented output, timing, or interface chrome.
- Use flat surfaces, purposeful borders, and small opacity or transform transitions. Do not hide essential content behind animation or playback.
- Homepage scroll motion lives in `src/styles/motion.css` and follows the rules in `DESIGN.md` under "Scroll motion". `tests/browser/motion.spec.ts` enforces them.
