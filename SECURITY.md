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
ambient environment proxies, bounds responses, and treats remote MCP content and metadata as
untrusted data.

Web tools honor an operator-configured outbound proxy, taken from the conventional proxy environment
or from `--http-proxy`/`WORKCELL_MCP_HTTP_PROXY`, and covering `webfetch`, every `websearch` backend,
and source icons. The selection is a startup snapshot: it is read once, and because a shell command
changes only its own children's environment, no tool call can influence it. A malformed value stops
startup instead of falling back to a direct dial.

The `shell` tool forwards the conventional proxy variables to every command it runs, verbatim and
including credentials, because withholding them makes every network-using command fail closed inside
a guest whose only egress is an enforcing proxy. A credentialed proxy URL is therefore readable by
any admitted command; prefer a proxy that authorizes the source over one that requires a password in
the URL. The Workcell-specific settings do not constrain a shell child, and `--no-http-proxy` does not
remove an ambient variable from a command's environment. `execution.networkAccess` reports `proxied`
when such a variable is observed, disclosing presence only.

A proxied request is not resolved by Workcell. Scheme, URL-credential, special-use-hostname, and
IP-literal policy still reject targets locally before the proxy is contacted, but the address
decision for a hostname, including DNS rebinding defense, is delegated to the proxy. Deploy this only
with a proxy that re-checks the resolved address before dialling. Hosts matched by a bypass rule keep
the full direct path, including resolution and connector pinning.

Provider origins are HTTPS, so a non-intercepting proxy observes only the `CONNECT` target and never a
search credential. An intercepting proxy trusted by the container's certificate store terminates TLS
and can read provider credentials and fetched content; that is an operator decision.

Source-icon lookup is disabled by default. `--web-icons` or `WORKCELL_WEB_ICONS=true` opts in for both
web tools and may issue additional page, icon-link, and fallback favicon requests to public origins.
Disabled mode omits provider-supplied inline icon data as well as locally resolved icons.

File mutation authority is immutable process configuration. Without `--allow-write` the server omits
`file_write`, `file_edit`, and `file_apply_patch` from its catalog and does not route calls to those
names, and the filesystem crate denies direct native calls independently. No tool argument can relax
this, and the mutation schemas reject unknown fields so a stale argument cannot be dropped into an
unintended write.

Reviewed transfer adds the byte route `GET|POST /files` because file bytes do not fit bounded MCP tool
results. It requires an operator-owned `--transfer-root`, authenticated remote-host discovery, a
configured workspace root, write authority, and the transfer group on Unix. Without that setup the
capability and route are absent. Ordinary MCP tools remain available without transfer configuration.
The transfer methods use `POST /mcp`; no additional listener or control plane is involved.

The `/files?reviewed=v1&stage=...` and `download=...` IDs name bounded
server-held records bound to the principal, workspace generation, process, policy/catalog and cwd.
They never replace the bearer; every byte request must authenticate and repeat the cwd handle. Only
the exact reviewed selectors are accepted. Raw `path` queries, unknown parameters, mixed selectors,
and the removed `file_upload`/`file_download` tool names are refused without workspace writes or mkdir.
There is no raw-transfer fallback. Origin-bearing browser requests remain forbidden and credentials are removed by
authentication before dispatch. Neither tokens nor IDs/paths/queries enter request logs.

Upload bytes are anonymous private files, not workspace paths. Quotas reserve declared size before
admission and remain charged through active leases. Seal checks observed digest and length; execution
rehashes the content and consumes a typed prepared publication through the shared ledger and file
mutation lock. The preview names canonical target/ancestor scopes and parent staging effects. Only
regular files, nonsymlink parents, 0644/0755 metadata, and explicit absent/revision
preconditions are supported. Missing ancestors require explicit reviewed `createDirectories` entries;
the byte POST never creates them. The resolver remains `mcp-files`; descriptor-relative no-follow opens
reinforce its policy rather than adding server-side path resolution. No archive extraction, permission
policy override, or arbitrary destination in the byte POST is supported.

The private journal is process-locked and generation/principal scoped, with count and byte ceilings.
Publishing is durable before the effect, completed outcomes follow file/directory sync, and an
interrupted publication is indeterminate rather than retried or inferred successful from matching
bytes. Terminal records have finite retention; unknown history does not authorize a retry. Unresolved
records are not silently reclaimed. Every existing private record, including pending files, is
validated before recovery or cleanup; incompatible formats fail startup without migration, rewriting,
or deletion. Workspace temporary files may survive a process crash and need
operator reconciliation. There is no cross-file transaction or implicit undo.

Atomic no-replace publication refuses a destination created at the last instant. Replacement still has
a revision-check/rename race against external writers; descriptor anchoring prevents following a
rebound symlink but cannot stop an ancestor being moved. The mutation lock coordinates this server's
file, workspace, snapshot, and reviewed-publication paths, not shell children or other processes. Streaming
downloads validate a selected revision/digest and implement strong If-Match and single-range semantics
without full buffering. An external writer can still change an open inode during the stream. A client
must verify the complete digest and length before local publication. Same-UID hostile processes and
network filesystems that do not honor the required locking/rename/fsync semantics are outside these
guarantees. The descriptor explicitly does not advertise external-writer atomic replacement.

The optional `ai.workcell/remote-host` discovery extension is served only through authenticated
`POST /mcp` after the operator configures one server, workspace, workspace-generation, root-project,
and principal identifier. The stable generation identifies replacement or reset of the configured
workspace and is independent of the random process instance identifier. The identifiers are disclosure
labels, not users or tenant records, and the single bearer
still carries all process authority. Its current-directory handle is resolved once through the
filesystem confinement policy; the descriptor also exposes the resulting root-relative display path
and stable catalog and policy revisions. The environment revision reflects either the disclosed
startup snapshot or nondisclosure. Its custom prepare, execute, release, status, and
cancel methods remain on authenticated `POST /mcp` and require modern per-request extension
negotiation. Every preparation is bound to the process instance, principal, workspace, configured
generation, root project, immutable current-directory handle, catalog and policy revisions, tool
contract, and argument digest. Generation mismatches are rejected before an instance mismatch can be
treated as volatile state loss. Durable workspace, session, project-resource, and snapshot-store
identity consists of server ID, workspace ID, generation, resource namespace version, root-project ID,
and principal ID; instance ID is used only for volatile operation and watch loss detection.
Preparation resolves resource intents without executing the tool. A bounded volatile ledger gives
mutating execution one transition and retains its structured result for same-invocation retries;
expiry, release, count limits, and byte limits bound abandoned state. Cancellation uses the active
tool cancellation token, and bounded ordered shell progress is retained without replacing live MCP
progress delivery. A cancellation before dispatch has `sideEffectsPossible: false`; cancellation after
a file mutation, direct child, or Git mutation may have started has `sideEffectsPossible: true` and an `indeterminate`
status because process termination cannot retract or prove the absence of prior effects. Restart is an
explicit indeterminate boundary. The extension adds no endpoint,
control plane, tenant, lease broker, transfer ticket, or signing authority, and is never exposed over
stdio or an unauthenticated HTTP listener.

Remote websearch intent keeps provider connection authority separate from the exact normalized query.
The query is represented by a query-derived opaque resource ID and bounded display text matching the
embedded permission query, so authorization does not collapse query disclosure into generic egress.

The extension also serves resolve-directory, stat, deterministic paginated list/traversal, bounded
revision-bearing text reads, and deterministic paginated text search through that same authenticated
MCP route. Every relative request repeats the immutable host and root-project binding and names an
opaque cwd handle. Handles are server-held directory bindings rather than path-bearing tickets;
resolving a directory creates a fresh one, and rebinding, removal, root escape, or an escaping symlink
is rejected. Pagination cursors are bounded server-held records bound to both request digest and
observed revision, so tampering is invalid and changed results are explicitly stale. Search preserves
bounded traversal coverage and truncation metadata even when no page cursor remains. Text reads expose
byte continuation within a selected line range, the line containing the first byte, and the last line
completed by the chunk, so oversized lines and beyond-EOF ranges cannot silently skip or fabricate
content.

Workspace watch state is volatile, bounded, and owned by the authenticated remote-host instance. A
subscription is bound to the same host, workspace generation, principal, root project, and immutable
cwd handle as its open
request. Native watcher paths pass through filesystem confinement and are converted to root-relative
POSIX paths before entering the retained event ring; protected paths and absolute host paths are not
disclosed. Raw callback queues, subscription count, retained event count and bytes, total events,
poll size and wait, lifetime, and tombstones are bounded. Close, expiry, process drop, and terminal
resync remove the native watcher. Restart, cursor tampering, kernel or process queue overflow, backend
failure, and retention loss return an explicit `fullResync` response rather than an incomplete stream.
Every subscription owns one abortable expiry task; close, terminal resync, replacement, expiry, and
host drop abort it instead of retaining a detached sleeper.
The backend's order is retained, but rename pairing is not portable: known ends become remove/create,
and ambiguity becomes `rescan`.

Project-asset discovery is a fixed, versioned server allowlist. It covers recognized project
instruction files, one-level skill `SKILL.md` sources in the documented compatibility directories,
`.caudra/workflows/*.rhai`, immediate Markdown files in exactly `.caudra/commands`,
`.claude/commands`, and `.opencode/commands`, and the exact `.caudra/permissions.toml` source; it has no
project-controlled extension mechanism. Discovery and reads reuse confined traversal, stat, symlink
rejection, protected-path policy, text bounds, and content revisions. Discovery admits at most 256
assets while scanning at most 50,000 entries, 16 MiB of retained path state, and 64 MiB of hashed
content; paths are at most 4,096 bytes and reads at most 64 KiB. A bounded partial scan is rejected.
`.env`, `init.lua`, MCP configuration, plugin configuration or source, general or remote configuration,
and arbitrary scripts are excluded. The server does not parse, load, execute, or grant trust to any
discovered source. Instructions, skills, and commands are labeled `declarative`; workflow bytes are
labeled `clientApprovalRequired`. Permissions are labeled `mixedReviewRequired`, allowing a client to
apply denies immediately while requiring review of allows against the returned revision.

Prepared workspace create, revision-matched write, mkdir, revision-matched rename, and
revision-matched delete batches use the existing operation ledger and its idempotent invocation
response. Workcell cannot make arbitrary multi-file publication atomic. It validates the complete
batch before publishing, uses atomic same-filesystem replacement or rename for each action, and keeps
an in-memory reverse rollback journal for failures and cancellation. A rollback failure is reported as
partial failure. Process termination, kernel failure, or host loss between publications or during
rollback is a crash boundary that can leave a partial batch; no durable recovery journal is claimed.
Rename and delete bind regular files by byte digest and file identity, including binary files. Text
writes and edits retain the UTF-8 and binary-content gates.
Direct non-interactive exec is likewise prepared into the common ledger, exact-cwd and option bound,
and admitted only through the immutable shell policy before process creation. Its output progress and
cancellation are the same bounded mechanisms used by the ordinary shell tool. Discovery advertises
these implemented subcapabilities as `v1`.

SCM discovery and every repository-relative path start from the same confined immutable cwd/resource
handles as workspace reads. A repository is accepted only when its ordinary worktree and `.git`
directory canonicalize inside the configured root. `.git` files, symlinked or external git
directories, linked worktrees, submodules, bare repositories, and common-directory indirection are
rejected. Repository handles are process-local bindings rather than path tickets. Status, history,
diff, and side responses are revision-bearing and bounded; paginated responses use opaque cursors
bound to the request and observed repository revision. Untracked and conflicted paths remain explicit
states rather than being folded into a generic dirty bit.

Repository discovery, identity, object decoding, and history traversal use `gix`. Commit object headers
are checked against the advertised 1 MiB per-object and 16 MiB aggregate `maxLogScanBytes` limits before
each traversed body is loaded or decoded; commits skipped to reach a cursor consume the same aggregate
budget. An aggregate stop is returned as a truncated response without a continuation cursor. Shallow
boundaries are read only from the regular, non-symlink `.git/shallow` file; shallow-path configuration
overrides are rejected. The reader accepts at most 1 MiB and 10,000 strict object IDs before traversal.
Before `gix` parses repository configuration, Workcell reads `.git/config` through a confined,
non-symlink descriptor under the advertised 1 MiB `maxConfigBytes` limit. It retains content and file
identity and revalidates them immediately before and after `gix` reopens the file; a race is stale.
Status uses Git's documented porcelain-v2 NUL format, changed-path discovery uses NUL-delimited
name-status, and textual patches are parsed into typed line records under hard source bounds. Raw stdout
and stderr are never
returned. The fixed Git child templates disable hooks and filesystem monitors, suppress prompts,
ignore global/system configuration, remove helper-affecting environment variables, and reject
repositories with external filter or diff-driver configuration. Every diff pass disables external
diff, textconv, and color independently of repository attributes. Remote-host construction first
bounds and validates `git --version`; an unavailable or invalid Git executable removes the SCM
capability and is reported through `controlPlaneMissing`.
There is no client-selected command, option, environment, executable, or generic Git route.

Stage, unstage, and discard are exact prepared operations in the existing ledger. A mutation accepts at
most 127 paths so its repository intent plus all path intents fit the 128-intent ledger bound.
Preparation records
the repository identity, HEAD, index, content-sensitive worktree revision, normalized path set, and
status preview. Execution serializes against filesystem mutations, checks the index lock and every
captured revision again, and reports stale, locked, cleanly cancelled, indeterminate post-start
cancellation, or failed outcomes structurally.
Retries with the same invocation ID reuse the retained outcome. Discard invokes only tracked-worktree
restore for the prepared paths; it does not remove untracked content and no clean/reset operation is
available. A process crash during a Git index update remains a repository recovery boundary.

Snapshot support is an explicit authenticated-HTTP, writable-files opt-in. `--snapshot-root` or
`WORKCELL_MCP_SNAPSHOT_ROOT` must name an existing absolute directory outside the configured workspace.
Startup rejects a symlink component, ownership by another identity, group/other access on Unix, a path
that contains the workspace or is contained by it, an unsupported private entry, an oversized journal,
or more journals than the recovery bound. Workcell never falls back to a shared temporary path. The
private root is operator state: do not mount it into the exposed workspace or serve it independently.

Capture traverses only canonical entries admitted by filesystem confinement and the protected-path
policy. Git metadata, `.workcell`, credential-bearing paths, the private snapshot root, and a configured
in-workspace code-worker cache are excluded. Each scan bounds aggregate directory-entry count and
retained path bytes before collecting entries, including wide directories and empty directory trees. A
configured exclusion is resolved through its nearest
existing ancestor, so a later-created suffix remains excluded; escaping and malformed suffixes fail
startup. Every included entry must remain a regular file; symlinks, sockets, devices, and pipes fail
capture. Per-file, file-count, total-byte, concurrent-capture, retained snapshot, journal, metadata, and
total-storage limits are fixed and advertised. Blob, manifest, checkpoint, and journal bytes are
serialized and charged prospectively, including replacement size, under one publication lock. Failed
or inconsistent captures run bounded reachability collection. A complete second scan must match the
first before an immutable manifest is published. Blob names are content digests and both blobs and
manifests are verified when read. Private files use owner-only modes and same-directory
create/sync/rename or create/link/sync publication.

Restore authorization happens against stable restore and deterministic pre-restore snapshot IDs and
the complete prepared create/replace/delete/conflict and missing-ancestor list in the existing operation
ledger. The workspace
revision is compared again before the first effect and each file is compared immediately before its
effect. An aggregate private-store write intent covers the prepared pre-restore manifest and blobs. A
mismatch refuses publication; it never chooses the snapshot over a later edit. The
pre-restore state is captured before a durable journal under that same restore ID enters `publishing`.
Execution creates only prepared ancestors and journals directory progress before file progress. Each
file replacement is atomic, but the complete restore is not. Termination can happen after a directory
creation or file rename and before its journal update. Startup therefore compares bounded journal
entries with pre-state and target digest/mode, marks safely completed work complete, and otherwise
reports `partial` or `indeterminate` with reconciliation required. It does not replay an incomplete
restore.

Unrevert targets the exact private pre-restore snapshot and uses the same prepared execution and status
path. Until a completed restore is acknowledged, overlapping restores are rejected. Partial and
indeterminate journals cannot be acknowledged and continue to gate overlap until an operator
reconciles private state. Journal count and byte limits are enforced before restore and unrevert; only
acknowledged terminal journals are reclaimable under pressure. Prepared cleanup also uses the common
ledger. It retains the exact checkpoint, acknowledged-journal, manifest, and unreachable-blob deletion
set and binds one server-state resource intent to that plan. Execution revalidates equality, removes
references before referents, and never widens the prepared set. Startup bounded GC makes every cleanup
crash boundary openable and permits GC-only recovery when no manifest ID remains. Pending preparations
and every non-reclaimable journal remain reachability roots, so cleanup cannot remove state needed for
restore, recovery, or unrevert.

Discovery reports snapshots only after private-store validation and startup recovery succeed. It sets
`controlPlane: true` only when operations, workspace reads, watch, project assets, writable prepared
mutation, direct exec, SCM, and snapshots are all enabled; otherwise `controlPlaneMissing` identifies
the absent slice. This is a capability summary, not a deployment controller. Snapshot methods add no
route, user, tenant, signed ticket, bearer, or authority beyond authenticated `POST /mcp`.

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

The optional filesystem indexer parses strict UTF-8 source in-process. Calls read through the normal
filesystem policy, then run tree-sitter and the bundled first-party extractor logic in `spawn_blocking`
under a process-wide two-permit semaphore. Source bytes bound parser input, and an absolute deadline
and cancellation cover blocking-pool queueing, construction, inspection, extraction, and formatting.
Node-count and depth limits are enforced by post-parse inspection before extraction; tree-sitter does
not expose a construction-memory ceiling for those limits. Syntax trees and native extractor state are
dropped after each call; no parser cache or scripting runtime is retained. These controls limit
accidental and adversarial work but do not turn native parser code into a process-isolated sandbox.

The code-graph tool group reads through the same confined filesystem group and adds no write
authority, no second path resolver, and no network access. It parses a whole tree rather than one
file, so its bounds are wider and all of them are host-owned: files per map, traversal entries,
single-file and total source bytes, a crawl deadline, per-file and whole-tree definition and
reference ceilings, PageRank iterations, retained cache entries and bytes, result rows, and
extraction worker count. Parse trees are dropped as each file's facts are extracted, so live memory
is bounded by worker count rather than by tree size. Extraction and ranking run on a blocking task
with their own bounded worker pool, which limits concurrent parser work but, exactly as for the
indexer above, is not CPU or memory containment. Every bound that fires is named in the result rather
than applied in silence, and reference counts are floors: a zero means none was found, never that
none exists.

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
  applies the same authorization decisions `file_read` applies, so a host never authorizes against a
  view filtered by confinement or protected-path policy. Traversal does skip directories holding
  regenerable build output by name, which is a relevance filter rather than an authorization one:
  those paths remain readable, and naming one as an explicit path enumerates it.
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
- Shell output filtering changes what a model reads, not what ran. A rule can omit output a reader
  would have judged relevant, and the corpus matches on program name, so a different program invoked
  under a matched name is rendered by that program's rule. Filtering is applied only to
  single-scope, non-opaque commands, success-summary rules are suppressed unless the command exited
  zero, and the unfiltered capture stays in the structured result. Use
  `--no-shell-output-filter` where the raw rendering is required for review.
- Filesystem authorization is path based and retains a potential time-of-check/time-of-use window under
  malicious concurrent filesystem mutation.
- A native host embedding the tool crates with an `_unconfined` constructor supplies the entire
  authorization layer. Workcell enforces bounded reads, writes, output, deadlines, and cancellation in
  that configuration, but no path or workdir boundary. A host defect there has the same reach as the
  process itself.
- Native document and image parsing occurs in-process. Internal bounds reduce risk but do not replace
  hard process memory and CPU isolation.
- Native source indexing and its feature-gated parser bundle also run in-process. Parser defects,
  construction-memory growth within the source bound, or bound-check defects can affect the server
  process despite per-call deadlines and post-parse limits.
- Credential-free Exa MCP search is not private or an availability guarantee. Queries leave the
  execution boundary, and normalized results can still contain inaccurate or malicious web content.
- A bearer token authenticates one process endpoint. It does not express per-tool, per-user, or
  per-request authorization. A token holder can select downloads and prepare/execute publications
  when reviewed transfer is configured. Client-side review is not a second server-side credential.
- Selected transfers check identity and content before publication or streaming, but cannot isolate
  the filesystem from external writers. Clients must verify downloaded length and digest before
  publishing locally, and replacement retains the revision-check/rename race described above.
- Monty is pre-1.0 software on a `0.0.x` line with a version-coupled worker protocol. Workcell pins the
  `monty-pool` dependency and the installed worker to the same release and they must be upgraded
  together; the build fails when the pins diverge and the pool reports any remaining skew as a fatal
  error on the first checkout. Treat interpreter escape as possible and do not rely on the code tool
  as the only barrier protecting anything sensitive to the deployment.
- An explicit `--code-worker` path is authoritative. Without one, discovery checks beside the server,
  then the verified embedded worker, then `PATH`. Embedded bytes are digest-checked and extracted into
  a private content-addressed cache under an interprocess lease. The `PATH` fallback still trusts the
  deployment's `PATH`; configure an explicit worker where it is not fully controlled. Set
  `--code-worker-cache` when the platform cache is unavailable, not writable, or not executable. The
  configured cache must be controlled by the Workcell process identity and not shared with less-trusted
  users.
- The `monty-pool` client links a TLS stack and a WebSocket implementation into the server binary to
  support a remote worker transport that Workcell never configures. Workcell only ever constructs the
  local subprocess transport, so that code is unreachable at runtime, but it is present in the binary
  and contributes third-party unsafe code that `forbid(unsafe_code)` in this workspace does not cover.
- The code worker's isolation comes from what the interpreter is not given, not from a kernel boundary.
  A defect in Monty's builtins or in Workcell's suspension handling could expose host capability that
  the design intends to withhold.
