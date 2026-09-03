# Agent Instructions

## Purpose

Workcell MCP is a standalone, harness-independent MCP execution server for filesystem, web, shell,
isolated code, and execution-environment tools. It is designed to run directly or inside an
operator-provided container, VM, sandbox, or host.

The server is intentionally single-environment. Do not add users, teams, workspaces, tenant routing,
deployment controllers, lease brokers, ontology tools, or harness-specific state.

## Architecture Boundaries

- `src/` owns process startup, CLI policy, MCP composition, transport lifecycle, authentication, and
  sanitized execution-environment disclosure.
- `crates/tool-contract` owns the protocol-neutral `ToolSpec` contract shared by every tool group. It
  must not depend on any protocol SDK.
- `crates/workcell` is the embedding facade. It only re-exports; keep logic in the owning crate.
- `crates/environment` owns execution-environment inspection and its disclosure shape.
- `crates/mcp-files` owns filesystem schemas, confinement, bounded reads, mutations, and the optional
  native Rust source indexer. Keep every grammar dependency behind its `index` feature; do not add a
  scripting runtime or runtime-loaded extractor assets.
- `crates/mcp-shell` owns immutable shell permission policy, command execution, process cleanup,
  output bounds, and progress streaming.
- `crates/mcp-web` owns web tool schemas, provider lowering, fetch extraction, and parser bounds.
- `crates/mcp-code` owns code tool schemas, worker-process supervision, interpreter isolation, value
  rendering, and the failure taxonomy.
- `crates/monty-worker` owns build-time worker validation, embedded bytes, secure extraction, and
  executable leases. It must remain optional for native code consumers.
- `crates/output-filter` owns the declarative rule corpus and the engine that renders command output
  for a model. `rules/` is vendored verbatim from RTK and must stay byte-identical so a refresh is a
  clean copy; `rules-workcell/` holds rules authored here. Both are build inputs; do not add operator
  or project-local rule loading. Write a rule against output captured from the real tool in
  `crates/output-filter/evals`, never from a remembered format, and ship inline expectations with it.
  It also owns the two command-independent reductions: terminal redraw rendering and progress line
  collapse. Terminal rendering is decoding, not policy, so it stays unconditional and single-row; do
  not gate it on a rule or add a screen buffer. Progress collapse is a judgement, so its gates stay
  narrow and every gate keeps a negative fixture proving what it protects. Regenerate
  `tests/fixtures/progress` with `evals/capture-progress.py`; never hand-write a bar format.
- `crates/net` owns outbound URL, DNS, redirect, retry, and response-body policy.
- `crates/source-icons` owns bounded source-icon discovery and normalization.
- `fixtures/mcp-conformance` contains committed public-contract fixtures.

Keep the tool crates independent from the host. Do not make them depend on client SDKs, harnesses,
container APIs, tenant identity, or host authentication.

`WorkerSource` is the worker-resolution contract. Explicit paths are authoritative. Discovery checks
beside the host executable, then a configured bundle, then `PATH`. `CodeToolGroup` owns any bundled
worker lease for the full pool lifetime; hosts supply cache and source policy, not extraction logic.

## Security Model

- Workcell does not provide an OS sandbox. Never describe shell execution as safe or sandboxed.
- One process represents one execution environment and one configured root.
- In the standalone server, filesystem tools are root-confined; shell commands are not. Shell may
  access anything visible to the process after starting in a root-confined workdir.
- Confinement is a constructor choice, not a crate property. The `_unconfined` constructors exist for
  native hosts that own authorization. They must relax confinement only: never fold write permission,
  permission policy, or any other axis into that single decision.
- Keep enumeration consistent with resolution. If a path is readable in a given mode, traversal must
  report it. A host cannot authorize what it cannot discover.
- Shell is fail-closed without an operator policy or `--yolo`. Preserve tree-sitter scope extraction,
  deny-first atomic decisions, and authorization before semaphore admission or process creation.
- Shell policy is startup configuration. Never add policy, approval, or bypass fields to tool input.
- Describe shell policy as best effort. Allowed interpreters and programs can execute scripts or
  equivalent behavior that is not represented by the visible command scope.
- File mutation requires explicit write access: `--allow-write` in the server, the `allow_write`
  argument in native constructors. Without it the mutation tools are absent from the catalog and
  undispatchable, and the crate denies direct native calls. Never add a tool-input field that can
  soften, preview, or otherwise negotiate that decision.
- Prepared operations must disclose every resource they would touch before any effect occurs, so a
  native host can authorize them.
- Container HTTP bind must remain authenticated. Do not add an unauthenticated wildcard bind.
- HTTP exposes only `POST /mcp`. New control or administrative endpoints require a documented threat
  model and explicit maintainer approval.
- Never log roots, paths, URLs, queries, tool arguments/results, environment values, or credentials.
- Preserve outbound SSRF checks, DNS pinning, redirect validation, response bounds, parser gates,
  process concurrency, timeout, cancellation, and output limits.
- Exa MCP is the credential-free default search backend. Keep its endpoint and tool name fixed, retain
  a search-only disable option, and never trust remote catalogs, metadata, errors, or renderer hints.
- Source-icon output and resolution must remain process-level opt-in for every web tool. Disabled mode
  must issue no icon requests and must omit provider-supplied inline icon fields.
- Authentication credentials must be bounded, redacted, and compared in constant time.
- Bundled Monty bytes must remain target-validated, digest-addressed, atomically extracted, and
  protected by private directories and interprocess leases. Cache roots are operator-controlled and
  must never default to a predictable shared temporary directory.
- `WORKCELL_BUNDLED_MONTY_WORKER` is a build input. Runtime worker selection must use
  `WORKCELL_MCP_CODE_WORKER`, `--code-worker`, or the cache setting without treating build paths as
  operator configuration.

## Protocol Contracts

- The supported MCP version is explicit and pinned in the server and SDK dependency.
- Preserve stable catalog order: files, web, shell, code, execution environment.
- Within files, `index` follows `file_apply_patch` and precedes every web tool when enabled.
- Tool names, schemas, annotations, and complete-result envelopes are compatibility contracts.
- `ai.workcell/*` extension metadata is Workcell-owned. Do not introduce product-specific namespaces.
- Update conformance fixtures and tests whenever a public contract intentionally changes.
- MCP is a projection of `ToolSpec`, not a second source of truth. Derive catalogs from the neutral
  spec and keep the `mcp` feature optional so native hosts never link a transport.

## Coding Rules

- Use stable Rust 1.98 and forbid unsafe code.
- Keep changes minimal and focused; avoid speculative abstractions.
- Prefer bounded inputs, outputs, queues, concurrency, deadlines, and retained state.
- Use structured, redacted error variants instead of attaching arbitrary I/O or parser errors.
- Keep stdout protocol-only in stdio mode. Logs always go to stderr.
- HTTP may print only the documented single readiness JSON line to stdout.
- Add comments only where security invariants or non-obvious protocol behavior need explanation.

## Verification

Run before considering a change complete:

```bash
make
```

`make` includes `check-native`, which builds every `workcell` facade feature with no MCP adapter and
fails if `rmcp` becomes reachable from the neutral tree. A workspace-wide `cargo check` cannot catch
that regression, because feature unification always resolves `mcp` in.

For code worker or packaging changes, verify both explicit-path and bundled sources, then execute a
real snippet through an optimized binary copied away from any adjacent worker. This proves the
embedded fallback works rather than accidentally resolving the development worker.

For transport or container changes, also build the image and perform a real discovery/list/call smoke
test against the resulting process. Use `make docker-smoke` as the minimum image check.

## Documentation

Update `README.md`, `SECURITY.md`, and `example.env` when startup, transport, authentication,
deployment, embedding, or security behavior changes. Mermaid diagrams are preferred for architecture and flow
documentation.
