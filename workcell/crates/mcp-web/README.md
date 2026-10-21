# workcell-mcp-web

`workcell-mcp-web` implements the `websearch` and `webfetch` MCP tools. It composes
`workcell-net` for outbound policy with `workcell-source-icons` for verified source icons while
keeping provider, clock, transport, icon, and PDF dependencies injectable for offline tests.

## Public API

`WebToolGroup` is the primary composition boundary:

```rust,no_run
use workcell_mcp_web::{WebToolGroup, WebsearchExecutionConfiguration};

let web = WebToolGroup::new(WebsearchExecutionConfiguration::default());
let catalog = web.catalog(2026);
assert_eq!(catalog.len(), 2);
```

Production composition uses `WebToolGroup::new` or `WebToolGroup::production`. Tests and alternate
hosts can use `WebToolGroup::with_dependencies` with `WebToolDependencies`.

Production source-icon resolution is disabled by default. Use
`WebToolGroup::production_with_source_icons(configuration, true)` only when the host has explicitly
opted in. Supplying an icon provider through `WebToolDependencies::new` is also an explicit opt-in;
`with_source_icons_enabled(false)` suppresses both remote inline icon data and local resolution.

## Websearch

Search provider selection is immutable process configuration, never model-controlled input.

- `WebsearchExecutionConfiguration::default()` uses Exa's credential-free hosted MCP endpoint. The
  process environment can select `exa-mcp`, disable search, or select another provider.
- The catalog uses a provider-specific Exa MCP, SearXNG, direct Exa API, Brave, Kagi, SerpApi Google,
  SerpApi Bing, or query-only diagnostic input schema.
- Backend-specific parameters are not advertised or accepted for another backend.
- Missing and invalid selected-backend configuration remains invokable and returns safe remediation
  guidance without issuing a network request.
- SearXNG supports API-key, bearer, basic, or credential-free operation.
- Credentialed SearXNG endpoints require HTTPS.
- Credential-free operator endpoints may intentionally resolve to private addresses.
- Exa MCP calls only the fixed `https://mcp.exa.ai/mcp` origin with `web_search_exa`. It sends no
  credential, disables redirects and ambient environment proxies, bounds JSON/SSE parsing, ignores
  remote MCP metadata, and normalizes only strict result fields. Search queries leave the process
  boundary.
- Direct Exa API remains available with an API key, a separate backend identity, a fixed HTTPS
  endpoint, and no credential forwarding to another origin.
- Brave uses its provider-owned Web Search endpoint with `X-Subscription-Token`, no redirects, and no
  ambient environment proxy. Country, language, pagination, freshness, and safe-search values are
  lowered to Brave-native query parameters.
- Kagi calls Kagi's first-party raw Search API with a bearer credential, exact JSON lowering, and no redirects.
- SerpApi is an optional scraping intermediary. Its operator-selected Google or Bing engine is not
  official Google or Bing API access. Redirects are disabled and credentials remain confined to the
  fixed SerpApi endpoint. Operators should assess unresolved legal and supply-continuity risks;
  Workcell does not provide legal advice.
- Ready providers implement one internal trait that owns catalog metadata, validation, lowering, and
  response extraction; common execution retains cancellation, icon enrichment, and output formatting.
- Time-range lowering uses an injectable clock.
- Results are URL-validated, deduplicated, field-bounded, count-bounded, and capped to 50 KiB of model
  text.
- The formatted result list is emitted only as the MCP model-facing content block. Structured output
  contains the canonical `results` array and does not duplicate it as `formattedResults`.
- Provider failures return the established successful error-shaped result without leaking secrets.
- Opted-in icon enrichment is best-effort, origin-deduplicated, batched, and cancellation-aware.

## Webfetch

- Only HTTP and HTTPS inputs are accepted; plain HTTP input is upgraded before public execution.
- Every DNS answer and redirect target is checked by `workcell-net` public-internet policy.
- With `WebToolGroup::production_with_proxy`, every outbound path in this crate honors the operator's
  proxy: webfetch, all search backends including the pinned provider origins, and source icons.
  Because provider origins are HTTPS, a non-intercepting proxy sees only the CONNECT target and never
  an API key. A proxied hop delegates address policy to the proxy, as described in `crates/net`.
- General responses are limited to 5 MiB and PDFs to 6 MiB.
- HTML supports raw HTML, readability-derived Markdown, and text extraction with bounded fallbacks.
- Scripts, styles, iframes, and framework payloads are removed from extracted output.
- PDF extract and attachment modes are supported; truncated PDFs are never emitted as attachments.
- Attachment filenames are decoded, sanitized, and byte-bounded.
- Structured previews are bounded while model output remains compatible with host retained-output
  handling.
- Already-fetched HTML is reused during source-icon discovery.

## Site rules

Some sites serve their HTML behind a script challenge but publish the same content as a feed or an
API. A site rule in `src/fetch/sites/` reads those instead. Each rule is one module implementing
`SiteRule::plan`, which turns a URL and format into ordered attempts: a target URL, request headers
(such as the site's User-Agent and Accept), the statuses that move on to the next attempt, and an
optional renderer that turns the response into Markdown or text. An attempt without a renderer goes
through the ordinary HTML, text, and PDF pipeline, so a rule that only rewrites a URL or changes a
header needs no rendering code.

- Every attempt URL passes the same URL policy as the caller's URL when the fetch is prepared, and
  every attempt shares the fetch's single deadline.
- The output keeps the caller's URL as `url`; `finalUrl` is the address that answered, and
  `extractionMethod` names the renderer, such as `reddit-rss`.
- Renderers run in the bounded blocking pool and must not panic on hostile input; a renderer error
  moves on to the next attempt, and on the last one fails the fetch.
- A rule may declare an icon URL so icon discovery never fetches the rate-limited page again.

To add a site, write `src/fetch/sites/<site>.rs` and list it in `RULES` in `src/fetch/sites/mod.rs`.

`reddit.rs` reads threads, comment permalinks, `redd.it` links, subreddits, user pages, and searches
from Reddit's Atom feed, and from its JSON API only when the feed answers 403. A 429 does not fall
back, since both share one rate limit; its error says when to retry. `format: "html"` and paths the
rule does not know are fetched unchanged.

## Parser Containment

HTML, PDF, search JSON, and image work run outside async executor threads behind a shared four-job
semaphore. Caller deadlines and cancellation stop waiting and retain the semaphore permit until the
blocking job actually completes, preventing unbounded detached concurrency.

This is bounded in-process parsing, not hard containment. `pdf_oxide` does not expose a complete memory
ceiling for compressed PDF objects, and `spawn_blocking` cannot kill a running parser. Hard CPU and
memory termination requires a separately resource-limited worker process.

Native `dom_smoothie` and `pdf_oxide` output can differ from TypeScript Readability/Turndown and
`unpdf` for complex layouts or unusual metadata. MCP shapes, limits, error classes, and reviewed normal
fixtures remain deterministic; parser-dependent output uses explicit invariants.

## Presentation Contract

The catalog emits trusted profile identifiers:

- `web.search.v1`
- `web.source.v1`

Workcell derives native presentation descriptors locally from bounded structured output. Remote MCP
renderer metadata is not trusted as UI authority.

## Verification

```bash
cargo fmt --all --check
cargo clippy -p workcell-mcp-web --all-targets -- -D warnings
cargo test -p workcell-mcp-web
```
