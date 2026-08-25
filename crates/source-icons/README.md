# workcell-source-icons

`workcell-source-icons` safely discovers, verifies, rasterizes, and caches source favicons. It returns
bounded PNG data URLs suitable for Workcell source descriptors; it never returns unverified remote
image bytes directly.

The resolver is directly callable, but the Workcell MCP host keeps source-icon resolution disabled by
default and invokes it only after an explicit operator opt-in.

## Resolution Pipeline

1. Validate the page URL with `workcell-net` public-internet policy.
2. Use caller-supplied HTML or fetch a bounded HTML prefix.
3. Parse quoted icon links and an optional HTTP(S) `<base href>`.
4. Score and deduplicate declared candidates by relation, size, type, and path.
5. Generate bounded directory fallback candidates when needed.
6. Probe candidates under a total deadline and request budget.
7. Validate image magic bytes rather than trusting response headers.
8. Decode with byte, dimension, pixel-area, and intermediate-memory limits.
9. Normalize the selected image to a bounded PNG data URL.
10. Store positive or definitive negative results in process-local LRU caches.

Supported input signatures are PNG, JPEG, GIF, WebP, ICO, and SVG.

## Public API

| API                        | Purpose                                                                |
| -------------------------- | ---------------------------------------------------------------------- |
| `SourceIconResolver`       | Resolver with injectable HTTP and process-local caches.                |
| `ResolveSourceIconOptions` | Page HTML, timeout, request, candidate, and output controls.           |
| `ResolvedSourceIcon`       | Verified source URL, PNG data URL, source kind, and cache diagnostics. |
| `resolve_source_icon`      | Convenience entry point using production defaults.                     |
| `clear_source_icon_caches` | Clears process-local positive and negative caches.                     |

Resolution failures are normally best-effort `None` results. Cancellation remains a distinct error so
callers do not incorrectly report a canceled web operation as successful. Transient network failures
are not inserted into negative caches.

## SVG Safety

SVG processing uses `resvg` with default features disabled. The parser and renderer disable system and
external fonts, filesystem resources, network resources, data hrefs, embedded raster images, and
external references. Input bytes, document dimensions, pixel area, tree complexity, pixmap memory, and
intermediate layers are bounded before publication.

## Network and Work Bounds

- All DNS answers and redirects use `workcell-net` policy.
- Declared candidates, fallback candidates, path segments, URL length, fetch operations, redirects, and
  total wall time are bounded.
- Icon requests disable retries and permit at most two redirects.
- Decode and raster work runs outside async executor threads behind a global concurrency limit.
- Output uses a size/quality ladder and never enlarges the source image.

The operation budget counts logical icon fetches; the total deadline is the hard caller-visible bound
across redirect exchanges. Native PNG encoding bytes are not a stable compatibility surface, so tests
compare MIME, dimensions, validity, and byte bounds.

## Verification

Tests are offline and cover discovery, fallback order, caching, cancellation, all raster signatures,
bounded SVG, adversarial paths, request budgets, deadlines, and transient failures.

```bash
cargo fmt --all --check
cargo clippy -p workcell-source-icons --all-targets -- -D warnings
cargo test -p workcell-source-icons
```
