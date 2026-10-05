# Contributing to Caudra

Thank you for helping with Caudra. Bug reports, fixes, documentation, and new features are all welcome. This guide shows how to set up a checkout, which checks to run, and what a pull request needs. If something in it is wrong or unclear, an issue about that helps too.

Caudra is an independent fork, developed at [github.com/caudra/caudra](https://github.com/caudra/caudra). Issues and pull requests go there.

## Before you start

Search open and closed issues before you open a new one. Someone may already be working on it.

A good bug report names the version you run (`caudra --version`), your operating system and terminal, what you did, and what you expected instead. Logs help a lot, and `caudra logs` prints the latest records. Logs leave out prompt text, model output, and tool input. Provider error messages are kept in full, so read the log before you paste it into an issue.

Do not report a security vulnerability in a public issue. [SECURITY.md](SECURITY.md) explains how to report one privately.

For a larger change, open an issue first and describe your plan. That covers new features, new dependencies, and changes to permissions, stored data, or config formats. That way we can agree on the approach before you spend time on it.

## Set up a checkout

### With Nix

[Nix](https://nixos.org/download/) gives you everything in one step. `nix develop` opens a shell with the pinned Rust toolchain, rust-analyzer, and every tool the checks use. If you use [direnv](https://direnv.net), the `.envrc` in the repository loads that shell when you enter the directory.

### Without Nix

Install these yourself:

- [rustup](https://rustup.rs). `rust-toolchain.toml` pins Rust 1.99.0, and rustup fetches it with clippy and rustfmt the first time you build.
- [just](https://github.com/casey/just) for the recipes in the `justfile`, and [cargo-nextest](https://nexte.st) for the tests.
- Python 3, which builds the worker behind the `python_execution` tool, and [ripgrep](https://github.com/BurntSushi/ripgrep), which some tests call.
- [stylua](https://github.com/JohnnyMorganz/StyLua), [ruff](https://docs.astral.sh/ruff/), [ty](https://github.com/astral-sh/ty), and [cargo-machete](https://github.com/bnjbvr/cargo-machete) for the formatting and lint checks.
- [Bun](https://bun.sh) 1.4.2 and [Node.js](https://nodejs.org) 24, but only for work on the website.

### First build

```sh
git clone https://github.com/caudra/caudra
cd caudra
just code-worker   # build the Python worker, once
just run           # start a debug build
```

Run `just code-worker` before any other recipe. The recipes point the build at the worker it produces, and they fail while that file is missing. The first run downloads and compiles the worker, and later runs reuse it. A plain `cargo build` works without the worker, but that binary has no `python_execution` tool and says so at startup.

A debug build keeps its own config, sessions, sign-ins, and logs in `caudra-debug` directories, such as `~/.config/caudra-debug/`. Your everyday Caudra keeps using its own, and the two share only project `.caudra/` directories. Sign in once inside the debug build with `just run -- auth login`. Arguments for Caudra always go after the `--`. To choose another directory name, set `CAUDRA_NAMESPACE`.

## Everyday commands

The `justfile` holds the commands that CI runs:

```sh
just check      # type-check every crate, without codegen
just lint       # clippy over every crate, warnings are errors
just test       # every test, under nextest
just fmt        # format the Rust, Lua, and Python sources
just gen-docs   # regenerate the generated docs
just ci         # most CI checks in one go, website included
```

`just check` and `just test` always cover the whole workspace, even if you add `-p`, and `just lint` does too. The full test suite takes a while, so run cargo directly while you work on one crate:

```sh
cargo check -p caudra-ui --tests
cargo clippy -p caudra-ui --tests -- -D warnings
cargo nextest run -p caudra-ui
```

The recipes set `WORKCELL_BUNDLED_MONTY_WORKER` to the worker from `just code-worker`. Export the same value in your shell. Then cargo and `just` build the same way, and switching between them does not trigger rebuilds:

```sh
export WORKCELL_BUNDLED_MONTY_WORKER="$PWD/target/code-worker/bin/monty"
```

Run tests with nextest rather than `cargo test`. nextest gives every test its own process, and some UI tests share global state that a single process would mix up.

Debug builds leave out debug info for dependencies and vendored C code, which keeps linking fast. Caudra's own crates keep it. To step into one dependency in a debugger, add `[profile.dev.package.<name>] debug = true` to `Cargo.toml`.

Before you ask for review, run `just ci`. Its website steps come last and need Bun and Node.js. CI runs them only when a change touches `site/`, the docs crates, or the installers. Only CI runs the browser tests, the macOS and Windows builds, and the Nix build.

## Where things live

| Path | What it holds |
|------|---------------|
| `src/` | The `caudra` binary: the command line, its subcommands, and startup |
| `caudra-*/` | The workspace crates, such as `caudra-agent` for the agent loop, `caudra-ui` for the terminal UI, and `caudra-providers` for model providers |
| `site/` | The website and the user docs, which also ship inside the binary |
| `plugins/` | Lua plugin sources, kept as examples and as coverage for the Lua API |
| `scripts/` | Build and maintenance scripts |
| `docs/` | Design records, each with a status line |
| `vendor/crossterm/` | A patched crossterm, kept until a release includes the fix that `Cargo.toml` describes |
| `tests/` | Integration tests for the binary |

The architecture section of [AGENTS.md](AGENTS.md) describes each crate in more detail.

## Writing code

[AGENTS.md](AGENTS.md) holds the full code guidelines. It is written for coding agents, and the same rules apply to everyone. In short:

- Write idiomatic Rust and keep it small. Every line should have a reason to be there, and a comment should explain only what the code cannot.
- Return `Result` instead of panicking. Use `thiserror` for domain errors, and `color_eyre` where the exact error matters less.
- Import types at the top of the file and use their short names. No wildcard imports.
- Put constants right after the imports, and avoid inline magic numbers and strings.
- Add a new dependency to the root `Cargo.toml` and use `workspace = true` in the crate. Check first whether an existing dependency already does the job.
- CI builds on Linux, macOS, and Windows, so keep platform-specific code behind `#[cfg(...)]`.

Put unit tests in a `#[cfg(test)]` module in the same file. Use `#[test_case]` for tables of cases, and give tests snake_case names. Test behavior and failure modes, and keep every test deterministic, which rules out sleeps. When a test checks an error or status message, compare it with a shared constant instead of a copied string.

## Things to keep in mind

- Work on an experimental feature stays behind its `[experimental]` switch. With the switch off, the feature shows no tools, commands, shortcuts, help entries, or status chips, and it does no work at startup.
- Do not add default key bindings that use Alt. By default, many macOS terminals do not send Option as Alt, so those keys would never arrive.
- Lua plugins are experimental too. When you change the Lua API, keep names close to Neovim's where that makes plugin code familiar. `just fmt` formats the plugins with stylua.

## Docs and the website

The user docs live in `site/src/content/docs/` as plain Markdown. The same pages are compiled into the binary for `/docs` and for the agent's `caudra-docs` skill. When a change alters what users see, update its page in the same pull request.

Some pages are generated by `caudra-docgen`: tools, providers, configuration, reference-configs, lua-api, plugins, keybindings, and commands. So are the `*.example.toml` files in `site/public/docs/` and the text between `<!-- caudra-docgen:NAME -->` markers in other pages. Edit the source and run `just gen-docs`, never the output. For a new setting, the source is its metadata in `caudra-config`, which also feeds `caudra config example`. CI fails when the generated files are out of date.

Tests resolve every docs link and section anchor, so update the links when you rename a heading. [site/AGENTS.md](site/AGENTS.md) holds the voice and structure rules for the docs. [site/DESIGN.md](site/DESIGN.md) covers the website design, and [site/RECORDINGS.md](site/RECORDINGS.md) explains how to record terminal footage for it.

The website is an Astro and Starlight app built with Bun. Run its checks from the repository root:

```sh
just site-install   # install dependencies from the lockfile, once
just site-dev       # local server that reloads on changes
just site-check     # type check
just site-test      # unit tests
just site-build     # production build into site/dist
just site-output    # tests against the build
just site-browser   # browser tests against the build
```

The browser tests need Playwright's Chromium, which you install once with `cd site && bunx --no-install playwright install chromium`. Native builds never need Bun.

## Working on Workcell

The file, shell, web, Python, and code-graph tools come from [Workcell](https://github.com/tensorninja/workcell-mcp), which lives in its own repository. `Cargo.toml` pins it to a Git revision.

To change Workcell, clone it next to this checkout as `../workcell-mcp`. The `just` recipes then build against your clone through `scripts/dev-cargo.sh`, which patches the dependency with a temporary lockfile. Plain cargo keeps using the pinned revision. For a scoped command against your clone, set the variable that the recipes set:

```sh
export WORKCELL_LOCAL="$PWD/../workcell-mcp/crates/workcell"
scripts/dev-cargo.sh nextest run -p caudra-workcell
```

While the clone is there, every recipe builds against it, unfinished work included. Keep it on a clean branch while you work on changes that only touch Caudra.

A Workcell change reaches release builds only after you push it and the pin moves. `just bump-workcell` moves the pin and updates `Cargo.lock` and the flake's dependency hashes. It needs `nix` on your PATH, but not the Nix daemon, and it refuses to run while `Cargo.toml`, `Cargo.lock`, or `flake.nix` have uncommitted changes.

## Nix

CI checks the flake when the Nix files, the Rust or Lua sources, or the docs change. If you edit `flake.nix`, run the same checks locally:

```sh
nix build         # release build with the Python worker embedded
nix fmt           # format the Nix files
nix flake check   # includes the Git dependency hash check
```

## Commits and pull requests

Branch from `main` and open your pull request against `main`. Keep each pull request to one change, so it is easy to review and to revert.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/) with a scope. Write the summary in the imperative and in lowercase, and use the body to explain why the change exists. A `!` marks a breaking change. Some examples from the history:

```text
feat(messaging): add consumer groups for topic work
fix(storage): keep SQLite's POSIX locks when opening caudra.db
docs(messaging): give cross-session messaging its own page
build: bump Workcell to 0a215f7
feat(ui)!: rename the default themes to caudra-dark and caudra-light
```

In the pull request, describe what changed and why, and link the issue. Say how you tested it. For a change to the terminal UI, a screenshot or a short recording helps a lot. If an AI agent wrote part of the change, say so and describe how you used it, with prompts where they help a reviewer follow the change.

CI runs on every pull request. It checks Rust and Lua formatting, runs clippy and the tests, checks the generated docs and unused dependencies, and builds on Linux, macOS, and Windows. Changes to the website, the Python scripts, or the Nix files start their own workflows.

## What reviewers look for

- Behavior that is explicit, and code that is easy to inspect.
- A small fix rather than a new abstraction.
- Tests for behavior and failure modes instead of implementation details.
- Non-obvious tradeoffs, explained in the pull request.
- Changes to permissions, stored data, network use, or compatibility, called out in the description and never hidden in a cleanup commit.

## License and attribution

Caudra is released under the [MIT License](LICENSE), and contributions are accepted under the same license. [NOTICE.md](NOTICE.md) records where the code comes from. If you adapt code from another project, keep its license: add the license text to `THIRD_PARTY_LICENSES/` and a note to `NOTICE.md`.
