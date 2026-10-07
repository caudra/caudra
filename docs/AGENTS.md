# Canonical docs

This directory owns the user documentation consumed by the binary and the separate website. Website design, recordings, and publishing rules belong to the website repository.

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
- Hand-written pages can hold generated regions between `<!-- caudra-docgen:NAME -->` and `<!-- /caudra-docgen:NAME -->`. Write around the markers, never between them. The `*.example.toml` files in `examples/` are generated too.

## Canonical format

- Every page under `content/` is compiled into Caudra. The agent reads it through the builtin `caudra-docs` skill, and users read it in the `/docs` modal. Rust reads the sources directly and must not require JavaScript tooling or a website build.
- Pages use YAML `title` and `description` metadata. Do not repeat the title as an H1 in the body. Rust supplies it for terminal/model rendering, and the website supplies it for the browser.
- `navigation.json` owns navigation groups and ordering for both consumers. List each non-index page exactly once. `index.md` is the plain-Markdown overview, not an HTML navigation manifest.
- Keep shared content ordinary Markdown. Do not use MDX imports, Astro components, or Starlight-only directives in bundled pages. Site-only presentation belongs in the website repository.
- Tests resolve every `/docs/<page>/#<anchor>` and `#<anchor>` link against the page headings, so renaming a heading means updating its links. Moving a source does not change its public URL or anchors. Example downloads remain at `/docs/<stem>.example.toml`.
- Search corrects a misspelt word only while that word appears nowhere in the docs. Never write a misspelling as an example.
- The TUI renders no inline HTML. It keeps the text inside a tag, shows a `badge` span as inline code and `<br>` as a space, and prints blockquotes as plain lines.
- An HTML comment on lines of its own, such as a `caudra-docgen` region marker, is dropped before the modal or the model sees the page. A comment inside a paragraph is not dropped, so never put one there.

## Product identity

- Lead with what Caudra does: effective action from coordinated models, tools, and subagents, adapted to each outcome and kept under user control. Prefer concrete actions and outcomes over claims about intelligence. Never imply the agent always knows the right answer.
- State that Caudra is an independent fork when project provenance is relevant.
- Use `Caudra` for the product and `caudra` for commands, paths, packages, APIs, and the lowercase wordmark.
- Use only `https://caudra.ai` for the site, docs, and installer origin. An unrelated apparel business owns `caudra.com`, so never imply any connection to it.
- Use `github.com/caudra/caudra` for source and `github.com/caudra/config` for the example config. Workcell source lives at `https://github.com/caudra/caudra/tree/main/workcell`.
- Telemetry names use the `caudra.*` namespace.
- Mark every experimental capability as experimental. Never present one as a default.
- Wherever Claude subscription sign-in appears, show that it is experimental and that Anthropic's terms limit Pro and Max subscriptions to official clients. Present ChatGPT, Copilot, and xAI sign-in plainly.
- Never call the Python worker a sandbox. Tool claims follow the generated Tools page.
