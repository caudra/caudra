# Security Policy

## Reporting

Report suspected vulnerabilities privately through GitHub Security Advisories for
`tensorninja/workcell-mcp`. Do not open a public issue for an unpatched vulnerability or include live
credentials, private paths, or exploit data in public logs.

## Boundary

Workcell is an execution server, not a sandbox. Filesystem and shell tools run with the operating-system
identity, mounts, network, capabilities, limits, and credentials granted to the Workcell process.
Operators are responsible for providing the intended isolation boundary.

When the web tool group is enabled, `websearch` defaults to the credential-free hosted Exa MCP service
at `https://mcp.exa.ai/mcp`. A search call sends the query and network metadata outside the Workcell
boundary. Exa is a third-party availability, privacy, terms, and supply-chain dependency and may apply
anonymous rate limits. Set `WORKCELL_WEBSEARCH_BACKEND=disabled` to retain `webfetch` without search
egress, or configure another backend. Workcell uses a fixed HTTPS origin, disables redirects and
environment proxies, bounds responses, and treats remote MCP content and metadata as untrusted data.

Source-icon lookup is disabled by default. `--web-icons` or `WORKCELL_WEB_ICONS=true` opts in for both
web tools and may issue additional page, icon-link, and fallback favicon requests to public origins.
Disabled mode omits provider-supplied inline icon data as well as locally resolved icons.

Shell requests are parsed into command scopes before execution. Without `--shell-policy` or `--yolo`,
all shell requests are denied. An explicit deny rejects the entire request before any command starts;
`--yolo` permits unmatched classified scopes but does not override a deny. If deny rules exist, opaque
syntax fails closed because Workcell cannot prove that a hidden executable is unmatched. This is an
application policy layer, not an OS security boundary: allowed programs can still execute indirect
behavior, so isolation remains mandatory for untrusted commands.

Execution-environment disclosure performs fixed, bounded local probes at startup and whenever the
`execution_environment` tool is called. A non-root Unix process actively runs
`sudo -n -- <resolved-true>` during each inspection; this may create audit records, update external
policy state, refresh the sudo credential timestamp and extend cached authorization lifetime, or invoke
local or remote PAM and sudo policy plugins. Success proves only that fixed command, while failure can
mean a password requirement or command-specific denial. A `not-found` result means sudo did not resolve
through the root-filtered `PATH`, not that no sudo binary exists elsewhere. Effective UID 0 may be
namespaced or container-confined and does not imply host-level root. These observations disclose
privilege-relevant capability but do not authorize shell use or bypass shell policy.

All executable probes resolve recognized programs through the process `PATH`, reject targets inside
the configured root, and execute accepted targets with fixed arguments; they do not pass
client-provided commands. Probe environments are cleared and selectively inherited, including a
`PATH` containing only canonical directories outside the configured root and no inherited home or
temporary-directory variables. Each probe starts from the resolved executable's parent directory
rather than the workspace. Output, individual processes, and the complete tool inspection have
deadlines; raw output is discarded after extracting normalized versions. Concurrent tool inspections
are serialized, cancellation waits for bounded cleanup, and Unix probes use dedicated process groups
for best-effort descendant termination. Operators must still treat installed executables as code and
must not treat reported availability or privilege, package-manager, container, sandbox, or network
classifications as an authorization or isolation boundary. Disable discovery and the tool together
with `--no-expose-execution-environment`.

Recommended controls for untrusted workloads include:

- A dedicated container, VM, microVM, or restricted operating-system account.
- Read-only root filesystems and narrowly scoped writable mounts.
- Dropped Linux capabilities and `no-new-privileges`.
- PID, CPU, memory, output, and wall-clock limits outside the process.
- Network egress policy appropriate to enabled web and shell behavior.
- No host socket, credential directory, SSH agent, cloud metadata, or broad secret mounts.
- Loopback publication or authenticated private networking for HTTP.

## Supported Deployment

The latest release is the supported security line. Linux is the primary production target. Stdio and
loopback HTTP are suitable for same-host clients. Container HTTP requires a process bearer token and
must be protected by the deployment network; Workcell does not terminate TLS.

Workcell serves MCP `2026-07-28` first and accepts exactly `2025-11-25` as a compatibility fallback by
default. Both HTTP eras remain stateless and POST-only: Workcell does not create protocol sessions,
issue `Mcp-Session-Id`, or enable legacy GET/DELETE lifecycle routes. Use `--modern-only` or
`WORKCELL_MCP_MODERN_ONLY=true` where accepting legacy request metadata and header semantics is not
appropriate. Protocol headers are routing and consistency checks, not authentication or authorization.

## Known Residual Risks

- Shell policy is syntactic, not confinement. An allowed command may use absolute paths, change
  directories, access the network, invoke other executables, or interpret dynamic input.
- An MCP client can write scripts through enabled mutation tools or shell output, then execute them
  through an allowed Bash, JavaScript, Python, Perl, or other interpreter. Policy checks the visible
  invocation, not script contents; denying one utility does not deny equivalent behavior implemented by
  another allowed executable.
- Filesystem authorization is path based and retains a potential time-of-check/time-of-use window under
  malicious concurrent filesystem mutation.
- Native document and image parsing occurs in-process. Internal bounds reduce risk but do not replace
  hard process memory and CPU isolation.
- Credential-free Exa MCP search is not private or an availability guarantee. Queries leave the
  execution boundary, and normalized results can still contain inaccurate or malicious web content.
- A bearer token authenticates one process endpoint. It does not express per-tool, per-user, or
  per-request authorization.
