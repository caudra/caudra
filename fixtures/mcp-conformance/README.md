# MCP Conformance Fixtures

These fixtures preserve the reviewed behavior carried into the Rust standalone MCP server. Tests may
consume fixtures but must never rewrite them implicitly.

Rust runners consume the filesystem and network cases. Provider cases record their owning
implementation in provenance; the retired direct TypeScript runtimes are not conformance authorities.

The corpus is language-neutral: JSON defines calls and assertions, while files under `assets/` are
opaque UTF-8 or base64 inputs. Paths in JSON are relative to this directory unless a fixture says
otherwise.

## Layout

- `catalog/v1/`: ordered MCP tool catalogs emitted by the Rust server.
- `filesystem/v1/`: independent filesystem calls with setup, normalized MCP output, and complete
  expected post-state.
- `network/v1/`: mocked search-backend normalization and webfetch dispatch cases.
- `assets/`: static bodies referenced by setup or mocked responses.

Every JSON case contains a positive integer `fixtureVersion`. Version 1 cases use the following
optional sections:

- `caseID`: stable, descriptive case identifier.
- `provenance`: implementation and test files reviewed to establish the expectation.
- `tool`: canonical tool name.
- `input`: model-provided arguments. A value shaped as `{ "$asset": "...", "encoding": "utf8" }`
  is replaced by the referenced text before schema validation and invocation.
- `setup.files`: root-relative files. `asset` is a corpus-relative UTF-8 input.
- `setup.mockResponses`: ordered mocked HTTP responses. `bodyAsset` is corpus-relative; an optional
  `bodyEncoding` of `base64` decodes the asset before constructing the response.
- `configuration`: non-secret execution configuration.
- `expected.contentText`: exact model-visible text.
- `expected.structuredContent`: structurally compared JSON output.
- `expected.isError`: expected MCP tool-error classification.
- `expected.postFilesystem`: complete, sorted root-relative file state after the call. Asset content
  is compared after UTF-8 loading.
- `expected.requests`: exact request order when source/tests establish it.
- `expected.requestIncludes` and `expected.requestExcludes`: request assertions for deliberately
  concurrent or fallback discovery.
- `expected.invariants`: allowlisted assertions where native image output is not byte-stable.
- `normalization`: explicitly allowlisted unstable fields.

Exact fixtures may normalize temporary roots, selected ports, request IDs, current-year placeholders,
injected-clock timestamps, and cache counters. They must not normalize semantic result fields, error
classification, tool schemas, descriptions, annotations, presentation profiles, or model-facing
text.

This initial corpus uses only `{{ROOT}}` and `{{CURRENT_YEAR}}`. A runner replaces the canonical
filesystem root with `{{ROOT}}` in every actual string before comparison. It replaces the year in the
websearch description with `{{CURRENT_YEAR}}` before catalog comparison. No wildcard or regular
expression normalization is implied.

For example, an actual structured field of `/tmp/run-a/notes.txt` becomes:

```json
{
  "path": "{{ROOT}}/notes.txt",
  "relativePath": "notes.txt"
}
```

Filesystem `contentText` is the normalized `structuredContent` serialized with
`JSON.stringify(value, null, 2)`. Newlines, field order, and omission of undefined fields are part of
the expectation. Read-only cases repeat their unchanged complete post-state so a conforming
implementation also proves it did not mutate the root.

The Kagi fixture covers its first-party raw API body, including region/date filters, boolean safe
search, timeout, redirect denial, and credential exclusion. Separate SerpApi Google and Bing fixtures
cover engine-specific lowering and provenance. SerpApi is an optional scraping intermediary, not
official Google or Bing API access; its legal and supply-continuity risks remain an operator concern,
not legal advice from this project.

The Exa MCP fixture covers the anonymous fixed-origin JSON-RPC request, SSE envelope parsing, strict
text-to-result normalization, and exclusion of remote MCP metadata. It is an offline contract fixture;
tests do not depend on the hosted service.

Websearch fixtures keep formatted model text in `expected.contentText`; structured output contains the
canonical result rows without a duplicate `formattedResults` field.

HTML readability and PDF extraction cases can use invariant expectations where native library output
is not byte-stable. Invariant cases must still assert MIME type, safety, bounds, required text
fragments, and the absence of unsafe content.

## Reviewed Boundaries

- The web runtime does not currently define MCP titles or annotations. `catalog/v1/web-tools.json`
  omits them rather than inventing values.
- PDF dispatch uses a fixed injected extractor result; native extraction is intentionally not
  byte-snapshotted.
- Redirect ports, request IDs, clocks, cache-counter snapshots, mutation failures, truncation limits,
  PDF attachment mode, and webfetch failure transport mapping are not covered by version 1.
