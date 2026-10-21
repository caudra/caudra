Caudra is the complete coding agent: steerable subagents, built-in token savings, a workbench, and a history you can revert, designed together in one native terminal binary. It coordinates models, tools, plugins, and subagents while keeping execution visible and under user control. Caudra is an independent fork at `github.com/caudra/caudra`; the hard-break release line starts at `0.1.0`.

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

- `make check` (or `cargo check -p <crate> --tests`)
- `make lint` - `cargo clippy --all --tests -- -D warnings`
- `make test` - `cargo nextest run --workspace`

Read `Makefile` for more. It requires GNU Make, Bash, and Python 3. On Windows, use Git Bash and install GNU Make separately. Python defaults to `python3`, overridable with `make PYTHON=python <target>`. Pass Cargo flags through `ARGS='...'`, such as `make build ARGS='--release'`. `run` passes arguments to Cargo, so use `make run ARGS='-- auth login'` for Caudra arguments.

Cargo-compiling targets depend on `code-worker` automatically. `make code-worker` is an optional warmup, not a required bootstrap. Keep build logic in Cargo and Python.

Dev and test builds keep only line tables and no incremental cache. For a debugger, set `CARGO_PROFILE_DEV_DEBUG=2`; to debug into a dep, set `[profile.dev.package.<name>] debug = true`.

Make uses `sccache` when it is installed (`scripts/cargo-cache.mk`); `SCCACHE=` opts out. Never share `CARGO_TARGET_DIR` between worktrees.

## Nix

`nix develop` (dev shell), `nix build`, `nix fmt` (nixfmt), `nix flake check` (includes git-dep-hashes drift).

## Architecture

Rust workspace, key crates in root dir:

- caudra-ui: Uses ratatui for an interactive UI (elm like architecture)
- caudra-workbench: Full-screen explorer/editor/source-control/search overlay, owns its own ratatui rendering
- caudra-providers: Integration with LLM providers via APIs (e.g. Anthropic, Z.AI, xAI)
- caudra-agent: An async agent loop that runs on smol
- caudra-interpreter: legacy Lua interpreter API using pydantic/monty
- caudra-storage: Persistent state across runs (e.g. sessions, auth)
- caudra-config: User config
- caudra-proxy: Explicit outbound proxy selection (`ProxyEnv`, `ProxyRoute`) for Caudra's own HTTP clients
- caudra-lua: Lua plugin system (API mirrored from neovim for plugin compatibility), built-in plugins in ./plugins dir
- caudra-acp: ACP ndjson stdio server
- caudra-workcell: Native Workcell adapter, Caudra authorization integration, and tool-result presentation
- caudra-docs: The user docs as data: pages, section addresses, the model index, the TUI display text, and full-text search. It embeds nothing. The binary embeds `docs/content` and `docs/navigation.json` (`src/docs.rs`, root `build.rs`), installs the `caudra-docs` builtin skill, and hands the same `Library` to caudra-ui for the `/docs` modal
- caudra-workflow: Durable workflow scripting engine (Rhai behind a `WorkflowEngine` trait), replay journal, and the neutral catalog/run/request types; the session manager, catalog discovery, storage actor, and native `workflow` tool live in `caudra-agent/src/workflow`
- caudra-automation: Runtime-neutral automation language (static `meta` header, args, events, untrusted values, host ABI, engine, validation, dry-run replay, schedules, limits), the `snapshot` read model, and the `request` types. `skill/SKILL.md` is the `caudra-automation-dev` skill, and `tests/examples/` holds the scripts its tests replay, which the skill and the docs page embed. Discovery, trust, the session runtime, and its storage glue live in `caudra-agent/src/automation`
- caudra-terminal-host: Everything Caudra tells the surrounding terminal: host detection (Herdr, then Orca, then the OSC 7501 probe), the pane status lane, OSC 9/BEL alerts, multiplexer wrapping, the routing table, and host worktrees and panes. A new host is one adapter module (`TerminalHost`, optionally `Workspaces`) plus one entry in `detect_from`. caudra-ui derives the `PaneStatus` and owns the stdout writer
- caudra-script: The Rhai sandbox workflows and automations share: a restricted engine, a header read without running the script, a bridge that serves host calls while the interpreter runs on its own thread, and canonical JSON and SHA-256 request digests

First-party Workcell tools are native Rust: file_read, file_glob, file_grep, file_write, file_edit, file_apply_patch, file_index, websearch, webfetch, shell, execution_environment, and the source-code pair source_code_context and source_code_refs.
Workcell's own isolated `code` tool stays in its standalone server and remote catalog, which Caudra checks
but never registers in place of its own `code`. Python runs in Caudra's native `code` tool (`caudra-agent/src/tools/native/code.rs`,
its description the source of truth) on Workcell's hosted pool only, always locally. Its gateway and
`HostModule` framework live in `caudra-agent/src/code/`: a module turns a call into a plan, and only the
gateway authorizes and runs it. MCP (`caudra-agent/src/mcp/code/`) and `history` are the modules. The
`history` module searches the FTS5 index in `caudra-storage/src/history_search/` through the rerank
funnel in `caudra-agent/src/history/`, which a UI history search can reuse.
Caudra owns authorization, registration, and presentation. Workcell owns protocol-neutral contracts,
validation, bounds, atomicity, network policy, subprocess cleanup, and the bundled Monty worker
lifecycle. Its crates live under `workcell/` in this workspace. Keep Workcell logic there rather than
duplicating it in `caudra-workcell`.

The source-code group is read-only and constructed separately from the writable file group, so its
limits clamp to the file group's. Envelope bounding uses Workcell's own `fit`/`Shrinkable`, and a
`GraphPhaseSink` bridges `GraphProgress` phases to `ToolLive::Annotation` for build feedback. A
`SelectorRefusal` is a successful call that returned no rows, not a tool error.

Web tools are built with `production_with_proxy`. `ambient_proxy()` reads `HTTP(S)_PROXY`,
`ALL_PROXY`, and `NO_PROXY` through `global_env_value`, so a project `.env` cannot set them. An
unparseable value degrades to direct dialling with a warning that never logs the URL.

Every isahc client Caudra builds for its own traffic sets its proxy explicitly with
`caudra_config::proxy_route(&url).apply(builder)` (or `https_proxy_route()` for fixed public HTTPS
endpoints), never libcurl's env lookup. `caudra-proxy` owns the selection: per-scheme variable,
then `ALL_PROXY`, uppercase first, numeric loopback always direct, and the bypass list always set.
A `curl` child gets the same settings through `ProxyEnv::configure_command`.

Automations' `http()` sends through `automation_http_client()` in `caudra-workcell`: Workcell's bounded
`HttpClient::request` with same-origin redirects and no retries, on one process-wide Tokio runtime
rather than a `WorkcellHost`, so every session kind sends from this machine. It shares
`ambient_proxy()` with the web tools and picks the public-internet or the private-network policy per
call from `[automations] allow_private_network`. Policy checks, secrets, and redaction stay in
`caudra-agent/src/automation/http.rs`; the client only sends.

`caudra-workcell` supplies Caudra's cache root and uses Workcell's bundled-only worker source for normal
production startup. `WORKCELL_BUNDLED_MONTY_WORKER` is a build input populated by Make targets using `code-worker`,
Nix, and release jobs. `WORKCELL_MCP_CODE_WORKER` is an authoritative process-only runtime override;
never persist it or silently fall back when it is invalid. Workcell's `CodeToolGroup` retains the
extracted worker lease for the complete pool lifetime.

Development and release builds use the same local Workcell workspace members and root `Cargo.lock`.
Edit `workcell/` directly. There are no sibling checkout overrides or separate Workcell revision bumps.

For worker or release changes, run the production bundled-worker execution test with a real pinned
worker, not only a catalog check. Release smoke tests must fail when `code` is reserved but
unavailable. Keep Monty's worker and `monty-pool` versions in lockstep.

Caudra's own tools are native Rust in `caudra-agent/src/tools/native`, registered from
`CAUDRA_NATIVE_TOOL_NAMES`: session work, orchestration, and interactive surfaces such as task,
workflow, code, tool_output, and view_image. They emit structured `ToolOutput` so the UI renders and
restores them in Rust, with no Lua round-trip on session load, click, or theme change.
`src/cmd::register_builtin_tools` is the single registration choke point for both native families.

No built-in Lua plugin loads in production: `ACTIVE_DEFAULT_LUA_PLUGINS` is empty. The sources in
./plugins stay in-tree as Lua-API coverage and as worked examples for plugin authors, and
`DEFAULT_BUILTINS` is what tests and docgen may load by name. The Lua host and its API are fully
supported for external plugins; only the built-ins moved to Rust.

## Config files

User-authored TOML configs upgrade themselves like `caudra.db` does (`caudra-config/src/migration/`):

- Renaming, moving, retyping or removing a key is a format change: add a `Step` to that file's `ConfigFormat` and raise `latest` in the same commit. Never add a fix-it error stub or a serde alias for an old spelling. A new optional key that older builds can ignore needs no bump.
- Read a config file through `migration::read` / `upgrade_text`, never `toml::from_str` on the raw file, so every parser sees the latest shape.
- Write a global file only through `atomic_file`, after `migration::ensure_current`. Never rewrite a project `.caudra/` file except from `caudra config migrate --project`.
- Backups go through `caudra_storage::backups` into the state dir's `backups/`, never beside the original.
- A test that writes a global config back must declare the current `version`, or the write is refused as `Pending`.

## Docs

The website is maintained separately and consumes the canonical docs from this repository. Application development, docs generation, and native builds require no JavaScript tooling or website build.

Canonical user docs live in `docs/content/*.md`, with YAML titles/descriptions and shared ordering in `docs/navigation.json`. Native builds and offline docs read these checked-in sources directly and must never depend on network access.

Canonical site and installer origin: `https://caudra.ai`. Configuration examples: `https://caudra.ai/docs/reference-configs/`.

Generated by `caudra-docgen` (`make gen-docs` / `make gen-docs-check`): tools, providers, configuration, reference-configs, lua-api, plugins, keybindings, commands, and release-notes (from `release-notes/*.md`, which are also the GitHub release bodies and what `/changelog` shows). It also writes `docs/examples/<stem>.example.toml` for every TOML config file (the references that reference-configs shows), and the text between `<!-- caudra-docgen:NAME -->` and `<!-- /caudra-docgen:NAME -->` markers in hand-written pages (`caudra-docgen/src/gen_regions.rs`): config key tables, and the automations page's patterns, read from `caudra-automation/tests/examples/`. Change the metadata in caudra-config (`example/`, `files.rs`, the `FIELDS` tables) or those example scripts, never the generated text.

Hand-written: quick-start, accounts, web-search, permissions, skills, mcp, cli, headless, acp, token-economy, context, memory, learning, sessions, review, workbench, worktrees, system-prompts, queue, notifications, telemetry, markdown, automations, and the docs index.

Style, tone, structure, and canonical format rules: `docs/AGENTS.md`. Read it before writing any docs. Website design and publishing rules belong to the separate website repository. Preserve public `/docs/` URLs and section anchors when moving sources.

## Release notes

A commit with a user-visible change (a feature, a fix, changed behaviour, or a step users must take) also updates the next release's notes: the newest `release-notes/*.md`, or a new file for the next version when `git tag -l v<its version>` shows the newest already shipped. Follow [Release notes](CONTRIBUTING.md#release-notes), then run `make gen-docs`.
