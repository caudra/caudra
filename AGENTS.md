# Agent Instructions

## Purpose

Workcell MCP is a standalone, harness-independent MCP execution server for filesystem, web, and shell
tools. It is designed to run directly or inside an operator-provided container, VM, sandbox, or host.

The server is intentionally single-environment. Do not add users, teams, workspaces, tenant routing,
deployment controllers, lease brokers, ontology tools, or harness-specific state.

## Architecture Boundaries

- `src/` owns process startup, CLI policy, MCP composition, transport lifecycle, authentication, and
  sanitized execution-environment disclosure.
- `crates/mcp-files` owns filesystem schemas, confinement, bounded reads, and mutations.
- `crates/mcp-shell` owns immutable shell permission policy, command execution, process cleanup,
  output bounds, and progress streaming.
- `crates/mcp-web` owns web tool schemas, provider lowering, fetch extraction, and parser bounds.
- `crates/net` owns outbound URL, DNS, redirect, retry, and response-body policy.
- `crates/source-icons` owns bounded source-icon discovery and normalization.
- `fixtures/mcp-conformance` contains committed public-contract fixtures.

Keep the tool crates independent from the host. Do not make them depend on client SDKs, harnesses,
container APIs, tenant identity, or host authentication.

## Security Model

- Workcell does not provide an OS sandbox. Never describe shell execution as safe or sandboxed.
- One process represents one execution environment and one configured root.
- Filesystem tools are root-confined; shell commands are not. Shell may access anything visible to the
  process after starting in a root-confined workdir.
- Shell is fail-closed without an operator policy or `--yolo`. Preserve tree-sitter scope extraction,
  deny-first atomic decisions, and authorization before semaphore admission or process creation.
- Shell policy is startup configuration. Never add policy, approval, or bypass fields to tool input.
- Describe shell policy as best effort. Allowed interpreters and programs can execute scripts or
  equivalent behavior that is not represented by the visible command scope.
- File mutation remains preview-only unless `--allow-write` is explicit.
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

## Protocol Contracts

- The supported MCP version is explicit and pinned in the server and SDK dependency.
- Preserve stable catalog order: files, web, shell.
- Tool names, schemas, annotations, and complete-result envelopes are compatibility contracts.
- `ai.workcell/*` extension metadata is Workcell-owned. Do not introduce product-specific namespaces.
- Update conformance fixtures and tests whenever a public contract intentionally changes.

## Coding Rules

- Use stable Rust 1.97 and forbid unsafe code.
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

For transport or container changes, also build the image and perform a real discovery/list/call smoke
test against the resulting process. Use `make docker-smoke` as the minimum image check.

## Documentation

Update `README.md`, `SECURITY.md`, and `example.env` when startup, transport, authentication,
deployment, or security behavior changes. Mermaid diagrams are preferred for architecture and flow
documentation.
