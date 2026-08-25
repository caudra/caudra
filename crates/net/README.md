# workcell-net

`workcell-net` provides shared outbound URL policy and bounded HTTP GET primitives for Rust Workcell
tools. It is intentionally separate from websearch and webfetch so source icons, redirects, and future
network tools use the same SSRF rules.

## Design

SSRF protection is a per-hop operation, not a one-time string check. `HttpClient` therefore separates:

- URL and hostname policy
- DNS resolution
- transport connection
- redirect handling
- body streaming
- retries, deadlines, and cancellation

The production transport disables automatic redirects and environment proxies. Each redirect is parsed,
resolved, revalidated, and connected through the reviewed addresses. Cross-origin redirects rebuild
headers from a safe allowlist so authorization, cookies, API keys, and custom credential headers are not
forwarded.

## Public API

The main types are:

| Type                       | Purpose                                                                          |
| -------------------------- | -------------------------------------------------------------------------------- |
| `HttpClient`               | Executes policy-checked bounded GET requests.                                    |
| `FetchOptions`             | Carries timeout, redirects, body limit, headers, retry policy, and cancellation. |
| `UrlPolicy`                | Selects public-internet or operator-configured trust semantics.                  |
| `OperatorConfiguredPolicy` | Explicit exceptions for trusted operator endpoints.                              |
| `DnsResolver`              | Injectable DNS boundary; production uses `TokioDnsResolver`.                     |
| `HttpTransport`            | Injectable wire transport; production uses `ReqwestTransport`.                   |
| `RetryPolicy`              | Bounded retry and backoff behavior.                                              |
| `BoundedResponse`          | Status, headers, final URL, bounded body, and truncation state.                  |

`HttpClient::public_internet()` is the default for model- or user-selected URLs. Operator-configured
policy is reserved for endpoints selected by trusted process configuration, such as a local SearXNG
instance.

## Public-Internet Policy

- Accepts only HTTP and HTTPS.
- Rejects URL credentials.
- Rejects localhost and special-use local names.
- Rejects IPv4 loopback, private, shared, link-local, benchmark, multicast, and reserved ranges.
- Rejects IPv6 loopback, unspecified, link-local, unique-local, multicast, and mapped non-public IPv4.
- Resolves all addresses and fails if any answer violates policy.
- Pins validated addresses into the production connector.
- Repeats validation for every redirect hop.

## Resource Bounds

- Bodies are streamed and stopped at a caller-supplied byte limit.
- A total deadline covers DNS, connection, redirects, retries, and body reads.
- Caller cancellation interrupts cooperative DNS and network work.
- Redirect counts and retry counts are explicit.
- `Retry-After` parsing is bounded.

DNS answer order, retry timing, and transport telemetry are not stable compatibility fields.

## Verification

Tests are offline and use injected resolver/transport implementations. Property tests cover generated
IPv4, IPv6, mapped-address, and hostname classification invariants.

```bash
cargo fmt --all --check
cargo clippy -p workcell-net --all-targets -- -D warnings
cargo test -p workcell-net
```
