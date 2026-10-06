# workcell-net

`workcell-net` provides shared outbound URL policy and bounded HTTP request primitives for Rust
Workcell tools. It is intentionally separate from websearch and webfetch so source icons, redirects, and
future network tools use the same SSRF rules.

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
| `HttpClient`               | Executes policy-checked bounded requests.                                        |
| `RequestSpec`              | Method, URL, optional body, `RedirectScope`, and `FetchOptions` for `request`.   |
| `RedirectScope`            | Whether a body-less GET may leave the origin; `get_url` uses `AnyOrigin`.        |
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

The crate re-exports `http` and `bytes`, so a host builds methods, headers, and bodies from the same
versions the client links.

## Methods and Bodies

`HttpClient::get` and `get_url` send a GET without a body. `HttpClient::request` sends any method with an
optional body of at most `MAX_REQUEST_BODY_BYTES` (1 MiB). A larger body fails with
`NetError::RequestBodyTooLarge` before DNS or any connection.

The client derives framing, connection, and proxy headers itself. A caller header named `Host`,
`Content-Length`, `Transfer-Encoding`, `Connection`, `Keep-Alive`, `TE`, `Trailer`, `Upgrade`, or
`Expect`, or any name starting with `Proxy-`, fails with `NetError::ReservedHeader` before DNS or any
connection. `get` and `get_url` apply the same check. The error names the header, never its value.

`RequestSpec::redirects` sets how far a request may be redirected:

- A GET without a body has nothing to replay. Under `RedirectScope::AnyOrigin` it follows redirects as
  described above, which is what `get` and `get_url` do. Under `RedirectScope::SameOrigin` it follows
  a 301, 302, 303, 307, or 308 only when the target has the origin of the hop that answered.
- Any other request, under either scope, follows only a 307 or 308 that stays on the same origin,
  re-sending its method, body, and headers, because those are the redirects that ask for the same
  request again and the origin is the one the caller chose.

A redirect that is not followed ends the chain and is returned with its bounded body as the final
response, and its target is neither resolved nor contacted. A redirect that would be followed beyond
`max_redirects` is an error. A body-less GET treats a redirect without a usable `Location` as an
error; any other request returns it. A followed hop is still validated, resolved, and pinned.

Only idempotent methods are retried: GET, HEAD, PUT, DELETE, OPTIONS, TRACE, and QUERY. Neither a
transport failure nor a retry status proves the server did nothing, so POST, PATCH, and any extension
method are attempted once whatever `RetryPolicy` allows.

Transport errors omit the request URL, whose path or query can carry a credential. The crate itself
emits no logs.

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

- Response bodies are streamed and stopped at a caller-supplied byte limit.
- Request bodies are capped at 1 MiB before any I/O.
- A total deadline covers DNS, connection, redirects, retries, and body reads. The deadline, not the
  error, decides what a failure is: a DNS, transport, or proxy failure that surfaces once it has
  passed is `NetError::Timeout`, which is never retried, whatever the failing stage called it. One
  that surfaces earlier keeps its own error, so an OS connect timeout inside the budget is a
  `NetError::Transport` that an idempotent request retries, or on a proxied hop a `NetError::Proxy`
  that it does not. A response that arrives after the deadline is still returned.
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
