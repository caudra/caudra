Caudra turns context into effective action. It is a terminal coding agent that coordinates models, tools, plugins, and subagents while keeping execution visible and under user control. Caudra is an independent fork at `github.com/caudra/caudra`; the hard-break release line starts at `0.1.0`.

## Code guidelines

- No trivial comments
- Minimal bloat (KISS, DRY, SRP)
- No unnecessary state (variables, fields, arguments)
- Each line of code should justify its existence
- Follow Rust idioms and best practices
- Latest Rust features can be used
- Descriptive variable and function names
- No wildcard imports
- Import types at top of file and use short names everywhere (e.g. `use std::sync::Arc;` then `Arc<T>`, never `std::sync::Arc<T>` inline)
- Keep consts at top of file, right after imports
- Explicit error handling with `Result<T, E>` over panics
- Use `color_eyre` when the specific error is not as important
- Use custom error types using `thiserror` for domain-specific errors
- Place unit tests in the same file using `#[cfg(test)]` modules
- Add dependencies to global `Cargo.toml`, and then set workspace=true in specific package
- Try solving with existing dependencies before adding new ones
- Prefer well-maintained crates from crates.io
- Be mindful of allocations in hot paths
- Prefer structured logging (wide logs with a bunch of useful fields)
- Provide helpful error messages
- Use #[test_case] when writing tests, and use snake_case for naming the tests
- No need for bullshit tests (e.g. tautology)
- Make sure tests are not flaky (no weird sleeps)
- No inline magic numbers or strings
- In tests const error/status messages and assert against the shared constant
- Add #[derive(Copy)] only on structs with 1 primitive field
- NO TRIVIAL COMMENTS

## Testing

Cheapest first, and scope to the crate you touched while iterating:

- `just check` (or `cargo check -p <crate> --tests`)
- `just lint` - `cargo clippy --all --tests -- -D warnings`
- `just test` - `cargo nextest run --workspace`

Read `justfile` for more.

Dev builds skip debug info for deps and vendored C (our crates keep it); to debug into a dep set `[profile.dev.package.<name>] debug = true`.

## Nix

`nix develop` (dev shell), `nix build`, `nix fmt` (nixfmt), `nix flake check` (includes git-dep-hashes drift, fixed by `just bump-workcell`).

## Architecture

Rust workspace, key crates in root dir:

- caudra-ui: Uses ratatui for an interactive UI (elm like architecture)
- caudra-workbench: Full-screen explorer/editor/source-control/search overlay, owns its own ratatui rendering
- caudra-providers: Integration with LLM providers via APIs (e.g. Anthropic, Z.AI, xAI)
- caudra-agent: An async agent loop that runs on smol
- caudra-interpreter: legacy Lua interpreter API using pydantic/monty
- caudra-storage: Persistent state across runs (e.g. sessions, auth)
- caudra-config: User config
- caudra-lua: Lua plugin system (API mirrored from neovim for plugin compatibility), built-in plugins in ./plugins dir
- caudra-acp: ACP ndjson stdio server
- caudra-workcell: Native Workcell adapter, Caudra authorization integration, and tool-result presentation
- caudra-docs: The user docs as data: pages, section addresses, the model index, the TUI display text, and full-text search. It embeds nothing. The binary embeds `site/src/content/docs` and `site/src/data/docs-navigation.json` (`src/docs.rs`, root `build.rs`), installs the `caudra-docs` builtin skill, and hands the same `Library` to caudra-ui for the `/docs` modal
- caudra-workflow: Durable workflow scripting engine (Rhai behind a `WorkflowEngine` trait), replay journal, and the neutral catalog/run/request types; the session manager, catalog discovery, storage actor, and native `workflow` tool live in `caudra-agent/src/workflow`
- caudra-automation: Runtime-neutral automation language (static `meta` header, args, events, untrusted values, host ABI, engine, validation, dry-run replay, schedules, limits), the `snapshot` read model, and the `request` types. `skill/SKILL.md` is the `caudra-automation-dev` skill, and `tests/examples/` holds the scripts its tests replay, which the skill and the docs page embed. Discovery, trust, the session runtime, and its storage glue live in `caudra-agent/src/automation`
- caudra-script: The Rhai sandbox workflows and automations share: a restricted engine, a header read without running the script, a bridge that serves host calls while the interpreter runs on its own thread, and canonical JSON and SHA-256 request digests

First-party Workcell tools are native Rust: file_read, file_glob, file_grep, file_write, file_edit, file_apply_patch, index, websearch, webfetch, shell, python_execution, execution_environment, and the code-graph family code_map, code_context, code_refs, code_impact, and code_expand.
Caudra owns authorization, registration, and presentation. Workcell owns protocol-neutral contracts,
validation, bounds, atomicity, network policy, subprocess cleanup, and the bundled Monty worker
lifecycle. Keep Workcell logic in Workcell rather than duplicating it in `caudra-workcell`.

The code-graph group is read-only and constructed separately from the writable file group, so its
limits clamp to the file group's. Envelope bounding uses Workcell's own `fit`/`Shrinkable`, and a
`GraphPhaseSink` bridges `GraphProgress` phases to `ToolLive::Annotation` for build feedback. A
`SelectorRefusal` is a successful call that returned no rows, not a tool error.

Web tools are built with `production_with_proxy`. `ambient_proxy()` reads `HTTP(S)_PROXY`,
`ALL_PROXY`, and `NO_PROXY` so web tools honour the same proxy shell children already inherit. An
unparseable value degrades to direct dialling with a warning that never logs the URL.

Automations' `http()` sends through `automation_http_client()` in `caudra-workcell`: Workcell's bounded
`HttpClient::request` with same-origin redirects and no retries, on one process-wide Tokio runtime
rather than a `WorkcellHost`, so every session kind sends from this machine. It shares
`ambient_proxy()` with the web tools and picks the public-internet or the private-network policy per
call from `[automations] allow_private_network`. Policy checks, secrets, and redaction stay in
`caudra-agent/src/automation/http.rs`; the client only sends.

`caudra-workcell` supplies Caudra's cache root and uses Workcell's bundled-only worker source for normal
production startup. `WORKCELL_BUNDLED_MONTY_WORKER` is a build input populated by `just code-worker`,
Nix, and release jobs. `WORKCELL_MCP_CODE_WORKER` is an authoritative process-only runtime override;
never persist it or silently fall back when it is invalid. Workcell's `CodeToolGroup` retains the
extracted worker lease for the complete pool lifetime.

Release builds use the Workcell Git revision pinned in `Cargo.toml`. Development recipes in `justfile`
patch its packages from the sibling `../workcell-mcp` checkout when that repository is present and use
a temporary lockfile seeded from `Cargo.lock`. Use plain Cargo when intentionally updating dependencies.

Because those recipes read the sibling checkout, a change made there is invisible to a release until it
is pushed and the pin moves. After pushing `workcell-mcp`, run `just bump-workcell`: it rewrites the
rev in `Cargo.toml`, updates `Cargo.lock`, and refreshes the flake's git dependency hashes, which are
keyed by commit and so change with every bump. Pass `--rev` to pin something other than the remote head.
It needs `nix` on PATH but not the daemon, and refuses to run on a dirty `Cargo.toml`, `Cargo.lock`, or
`flake.nix`. Local tests keep passing without it, so a Workcell change is not finished until it runs.

For worker or release changes, run the production bundled-worker execution test with a real pinned
worker, not only a catalog check. Release smoke tests must fail when `python_execution` is reserved but
unavailable. Keep Monty's worker and `monty-pool` versions in lockstep.

Caudra's own tools are native Rust in `caudra-agent/src/tools/native`, registered from
`CAUDRA_NATIVE_TOOL_NAMES`: tool_output_read, tool_output_grep, and view_image. They emit structured
`ToolOutput` so the UI renders and restores them in Rust, with no Lua round-trip on session load,
click, or theme change. `src/cmd::register_builtin_tools` is the single registration choke point for
both native families.

No built-in Lua plugin loads in production: `ACTIVE_DEFAULT_LUA_PLUGINS` is empty. The sources in
./plugins stay in-tree as Lua-API coverage and as worked examples for plugin authors, and
`DEFAULT_BUILTINS` is what tests and docgen may load by name. The Lua host and its API are fully
supported for external plugins; only the built-ins moved to Rust.

## Docs

The website is a self-contained Astro + Starlight application in `site/`, using Bun and a site-local lockfile. Run `just site-install`, `just site-dev`, `just site-check`, `just site-build`, or `just site-test`. Keep JavaScript dependencies and build output out of the repository root.

Canonical user docs live in `site/src/content/docs/*.md`, with YAML titles/descriptions and shared ordering in `site/src/data/docs-navigation.json`. Astro and Rust consume these checked-in sources independently. Native builds and offline docs must never depend on Bun, Astro output, or network access.

Canonical site and installer origin: `https://caudra.ai`. Canonical example config: `github.com/caudra/config`.

Generated by `caudra-docgen` (`just gen-docs` / `just gen-docs-check`): tools, providers, configuration, lua-api, plugins, keybindings, commands. It also writes `site/public/docs/<stem>.example.toml` for every TOML config file, and the text between `<!-- caudra-docgen:NAME -->` and `<!-- /caudra-docgen:NAME -->` markers in hand-written pages (`caudra-docgen/src/gen_regions.rs`): config key tables, and the automations page's patterns, read from `caudra-automation/tests/examples/`. Change the metadata in caudra-config (`example/`, `files.rs`, the `FIELDS` tables) or those example scripts, never the generated text.

Hand-written: quick-start, permissions, skills, mcp, cli, headless, acp, token-economy, context, sessions, review, workbench, worktrees, system-prompts, queue, notifications, telemetry, markdown, automations, and the docs index.

Style, tone, structure, and website workflow rules: `site/AGENTS.md`. Read it before writing any docs.
