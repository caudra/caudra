# workcell-mcp-code

`workcell-mcp-code` implements Workcell's `code_execution` MCP tool. It evaluates a bounded Python
snippet in a separate [Monty](https://github.com/pydantic/monty) worker process, returns the value of
the final expression as JSON alongside captured output, and converts interpreter failures into a
structured taxonomy with guidance aimed at an agent rather than a human reading a traceback.

Snippets never execute in the server process. The crate supervises a pool of worker processes over
Monty's subprocess transport, retires workers after a bounded number of checkouts, and replaces any
worker that aborts, so an out-of-memory kill or a stack overflow inside a snippet cannot take the
server down with it.

## Isolation

Isolation is the absence of capability, not a kernel boundary. Every interpreter suspension that would
reach outside the process is answered rather than forwarded:

- Filesystem and other OS calls are refused, surfacing as `PermissionError`, and the result names the
  filesystem or shell tool that the caller should use instead.
- Environment reads observe an explicitly empty environment, so no host variable is disclosed.
- Absent callables such as `eval` resolve to `NameError` rather than being satisfied by the host.
- There is no network access, no subprocess creation, and no session state between calls.

Bounds are compile-time constants and are never accepted as tool input: 64 KiB of source, a
caller-selected timeout capped at 30 seconds, a 256 MiB memory ceiling enforced by the worker's global
allocator, 256 KiB of captured output per stream, a suspension cap, and a concurrency limit of two.

The interpreter is Monty, not CPython, and implements an incomplete subset of the language. The tool
description enumerates the divergences that most often waste a caller's turn, and the fixture-backed
catalog test keeps that list from silently drifting.

## Worker binary

The crate does not build the worker as a Rust dependency. `WorkerSource` selects an authoritative
external path, a bundled-only worker extracted under a caller-provided cache root, or discovery.
Discovery checks beside the current executable, then the configured bundle, then `PATH`. Install an
external worker with `make code-worker`; release builds embed it, while the container image ships it
at `/usr/local/bin/monty` to remain compatible with its read-only filesystem and `noexec` temporary
directory.

The worker is installed from a pinned release rather than built as a workspace member. Monty's
interpreter and type checker pull a large ruff/ty tree that only resolves under Monty's own lockfile,
and workspace feature unification would enable `monty-proto/worker` for the server as well, linking
the interpreter into the binary Workcell ships. Because the wire protocol is version-coupled, the
`monty-pool` pin and the installed worker version must move together; `make code-worker` fails when
they diverge, and the pool reports any remaining skew as a fatal error on the first checkout.

## Verification

```bash
cargo clippy -p workcell-mcp-code --all-targets -- -D warnings
cargo test -p workcell-mcp-code
```

Integration tests need a worker binary and skip with an explicit message when none is found. Set
`WORKCELL_MCP_CODE_WORKER` to test against a specific build.

## License

Apache-2.0, consistent with the workspace. Monty is a separate MIT-licensed project by Pydantic. It
may be embedded as data, but is extracted and executed as a separate program rather than linked into
Workcell. See the repository's `THIRD_PARTY_LICENSES/Monty.txt`.
