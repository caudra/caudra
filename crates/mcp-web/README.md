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
  credential, disables redirects and environment proxies, bounds JSON/SSE parsing, ignores remote MCP
  metadata, and normalizes only strict result fields. Search queries leave the process boundary.
- Direct Exa API remains available with an API key, a separate backend identity, a fixed HTTPS
  endpoint, and no credential forwarding to another origin.
- Brave uses its provider-owned Web Search endpoint with `X-Subscription-Token`, no redirects, and no
  environment proxy. Country, language, pagination, freshness, and safe-search values are lowered to
  Brave-native query parameters.
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
- General responses are limited to 5 MiB and PDFs to 6 MiB.
- HTML supports raw HTML, readability-derived Markdown, and text extraction with bounded fallbacks.
- Scripts, styles, iframes, and framework payloads are removed from extracted output.
- PDF extract and attachment modes are supported; truncated PDFs are never emitted as attachments.
- Attachment filenames are decoded, sanitized, and byte-bounded.
- Structured previews are bounded while model output remains compatible with host retained-output
  handling.
- Already-fetched HTML is reused during source-icon discovery.

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
