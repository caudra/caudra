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

The code tool group is the one place where Workcell adds isolation rather than assuming it. Snippets
run in a separate `monty` worker process, never in the server process. The worker is given no
filesystem access, no network access, no ability to spawn processes, and an explicitly empty
environment, so `os.getenv` and `os.environ` observe nothing from the host and file access raises
`PermissionError`. Each call is fed a fresh interpreter state, so nothing persists between calls.
Snippets are bounded by a caller-supplied timeout capped at 30 seconds, a 256 MiB memory ceiling
enforced by the worker's global allocator, bounded captured output, and a cap on interpreter
suspensions. A worker that exhausts memory, overflows its stack, or otherwise aborts terminates only
itself; the supervising server replaces it. This is process isolation for a language runtime, not an
OS sandbox: the worker still runs with the identity and namespace of the deployment, so operator
isolation remains mandatory.

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

The statements above describe the standalone server. Workcell's tool crates are also embeddable
directly by a native Rust host through the `workcell` facade, and that host becomes the authorization
layer. Confined constructors (`FileToolGroup::new`, `ShellToolGroup::with_policy`) enforce exactly
what the server enforces. The `_unconfined` constructors do not, and they are the intended mechanism
for hosts that authorize paths and commands themselves:

- `FileToolGroup::new_unconfined` disables root confinement and protected-path denial together.
  Absolute paths and `..` traversal resolve anywhere the process can reach, and credential-bearing
  entries such as `.env`, `.ssh`, `.netrc`, `*.key`, and `id_rsa` are readable. Its `allow_write`
  argument independently controls mutation; pass `false` for inspection-only hosting. Broad traversal
  reports the same entries `file_read` will return, so a host can enumerate the full reachable set
  rather than authorizing against a filtered view.
- `ShellToolGroup::new_unconfined` and `with_policy_unconfined` relax only workdir resolution.
  Permission policy stays fail-closed unless the host supplies its own, and deny rules still reject a
  request before any command runs.

Prepared operations exist so that authorization can happen before any effect. `prepare_apply_patch`,
`ShellToolGroup::prepare`, and the web `prepare_*` methods disclose every path, command scope, query,
or URL a call would touch without reading, writing, or executing anything. A host that commits a
prepared value without inspecting its resources has performed no authorization, and unconfined mode
grants that call the full reach of the process. Embedding does not add an isolation boundary;
deployment isolation remains mandatory exactly as it is for the standalone server.

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
- A native host embedding the tool crates with an `_unconfined` constructor supplies the entire
  authorization layer. Workcell enforces bounded reads, writes, output, deadlines, and cancellation in
  that configuration, but no path or workdir boundary. A host defect there has the same reach as the
  process itself.
- Native document and image parsing occurs in-process. Internal bounds reduce risk but do not replace
  hard process memory and CPU isolation.
- Credential-free Exa MCP search is not private or an availability guarantee. Queries leave the
  execution boundary, and normalized results can still contain inaccurate or malicious web content.
- A bearer token authenticates one process endpoint. It does not express per-tool, per-user, or
  per-request authorization.
- Monty is pre-1.0 software on a `0.0.x` line with a version-coupled worker protocol. Workcell pins the
  `monty-pool` dependency and the installed worker to the same release and they must be upgraded
  together; the build fails when the pins diverge and the pool reports any remaining skew as a fatal
  error on the first checkout. Treat interpreter escape as possible and do not rely on the code tool
  as the only barrier protecting anything sensitive to the deployment.
- The worker binary is resolved at startup from `--code-worker`, then beside the server executable,
  then `PATH`. The `PATH` fallback trusts the deployment's `PATH`: an operator who leaves it writable
  by a lower-privileged account lets that account supply the process that receives every snippet.
  Configure `--code-worker` explicitly where `PATH` is not fully controlled.
- The `monty-pool` client links a TLS stack and a WebSocket implementation into the server binary to
  support a remote worker transport that Workcell never configures. Workcell only ever constructs the
  local subprocess transport, so that code is unreachable at runtime, but it is present in the binary
  and contributes third-party unsafe code that `forbid(unsafe_code)` in this workspace does not cover.
- The code worker's isolation comes from what the interpreter is not given, not from a kernel boundary.
  A defect in Monty's builtins or in Workcell's suspension handling could expose host capability that
  the design intends to withhold.
