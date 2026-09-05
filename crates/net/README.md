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

The production transport disables automatic redirects and ambient environment proxy detection. Each
redirect is parsed, resolved, revalidated, and connected through the reviewed addresses. Cross-origin
redirects rebuild headers from a safe allowlist so authorization, cookies, API keys, and custom
credential headers are not forwarded.

The route for each hop is chosen before any I/O. A direct hop resolves the hostname and pins every
approved answer into the connector. A proxied hop does neither, because the proxy performs the lookup
and therefore owns the address decision. `TransportRoute` makes that difference explicit so a direct
hop can never reach the wire without pinned addresses.

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
| `ProxyConfiguration`       | Immutable per-scheme proxy selection with `NO_PROXY`-style bypass rules.         |
| `TransportRoute`           | Whether a hop is dialled directly with pinned addresses or through a proxy.      |

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

## Proxied Hops

`HttpClient::with_proxy` routes matching hops through an operator-configured proxy. Under an enforcing
sandbox the guest cannot resolve names at all, so local resolution would fail closed before the proxy
was ever reached.

Every DNS-free check above still applies: scheme, URL credentials, special-use hostnames, and IP
literals are rejected locally, before the proxy is contacted. What a proxied hop delegates is the
address decision for a hostname, including rebinding defense. Deploy this only with a proxy that
re-checks the resolved address before dialling. Hosts matched by a bypass rule keep the full direct
path, resolution and pinning included.

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
