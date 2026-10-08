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
- GNU Make and Bash for the targets in the `Makefile`, and [cargo-nextest](https://nexte.st) for the tests.
- Python 3, which builds the worker behind the `python_execution` tool, and [ripgrep](https://github.com/BurntSushi/ripgrep), which some tests call.
- [stylua](https://github.com/JohnnyMorganz/StyLua), [ruff](https://docs.astral.sh/ruff/), [ty](https://github.com/astral-sh/ty), and [cargo-machete](https://github.com/bnjbvr/cargo-machete) for the formatting and lint checks.

On Windows, run Make from Git Bash and install GNU Make separately. CI provisions GNU Make explicitly. The Makefile uses `python3` by default. If your Python 3 executable is named `python`, use `make PYTHON=python <target>`.

Build logic stays in Cargo and Python. Application development, native builds, and docs generation do not require Bun, Node.js, or a website checkout.

### First build

```sh
git clone https://github.com/caudra/caudra
cd caudra
make run           # start a debug build
```

Cargo-compiling targets depend on `code-worker` automatically: `build`, `check`, `run`, `test`, `lint`, `install`, `install-fast`, `gen-docs`, `gen-docs-check`, and the Workcell targets that compile Rust. No manual bootstrap is needed. The first run downloads and compiles the worker, and later runs reuse it. Run `make code-worker` for an optional warmup. A plain `cargo build` works without the worker, but that binary has no `python_execution` tool and says so at startup.

A debug build keeps its own config, sessions, sign-ins, and logs in `caudra-debug` directories, such as `~/.config/caudra-debug/`. Your everyday Caudra keeps using its own, and the two share only project `.caudra/` directories. Sign in once inside the debug build with `make run ARGS='-- auth login'`. The `run` target passes `ARGS` to Cargo, so arguments for Caudra go after the `--`. To choose another directory name, set `CAUDRA_NAMESPACE`.

## Everyday commands

The `Makefile` holds the commands that CI runs:

```sh
make check      # type-check every crate, without codegen
make lint       # clippy over every crate, warnings are errors
make test       # every test, under nextest
make fmt        # format the Rust, Lua, and Python sources
make gen-docs   # regenerate the generated docs
make ci         # most application CI checks in one go
```

Pass extra Cargo flags with `ARGS='...'`, such as `make build ARGS='--release'`, `make check ARGS='--all-targets'`, or `make test ARGS='--no-fail-fast'`.

`make check` and `make test` always cover the whole workspace, even if you add `-p` through `ARGS`, and `make lint` does too. The full test suite takes a while, so run Cargo directly while you work on one crate:

```sh
cargo check -p caudra-ui --tests
cargo clippy -p caudra-ui --tests -- -D warnings
cargo nextest run -p caudra-ui
```

The Make targets set `WORKCELL_BUNDLED_MONTY_WORKER` to the worker from `make code-worker`. Before using Cargo directly, run `make code-worker` if no Make build has prepared it yet, and export the same value in your shell. Then Cargo and Make build the same way, and switching between them does not trigger rebuilds:

```sh
export WORKCELL_BUNDLED_MONTY_WORKER="$PWD/target/code-worker/bin/monty"
```

On Windows, use `monty.exe` in that path.

Run tests with nextest rather than `cargo test`. nextest gives every test its own process, and some UI tests share global state that a single process would mix up.

Debug builds leave out debug info for dependencies and vendored C code, which keeps linking fast. Caudra's own crates keep it. To step into one dependency in a debugger, add `[profile.dev.package.<name>] debug = true` to `Cargo.toml`.

Before you ask for review, run `make ci`. CI also runs the macOS and Windows builds and the Nix build. Website checks run in the separate website repository.

## Where things live

| Path | What it holds |
|------|---------------|
| `src/` | The `caudra` binary: the command line, its subcommands, and startup |
| `caudra-*/` | The workspace crates, such as `caudra-agent` for the agent loop, `caudra-ui` for the terminal UI, and `caudra-providers` for model providers |
| `docs/content/` | Canonical user docs, which also ship inside the binary |
| `docs/navigation.json` | Shared docs navigation groups and ordering |
| `docs/examples/` | Generated config examples, published at `/docs/*.example.toml` |
| `workcell/` | Workcell tools and supporting crates, built as local workspace members |
| `plugins/` | Lua plugin sources, kept as examples and as coverage for the Lua API |
| `scripts/` | Build and maintenance scripts |
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
- Lua plugins are experimental too. When you change the Lua API, keep names close to Neovim's where that makes plugin code familiar. `make fmt` formats the plugins with stylua.

## Docs

The user docs live in `docs/content/` as plain Markdown, with navigation in `docs/navigation.json`. The same pages are compiled into the binary for `/docs` and for the agent's `caudra-docs` skill. When a change alters what users see, update its page in the same pull request.

Some pages are generated by `caudra-docgen`: tools, providers, configuration, reference-configs, lua-api, plugins, keybindings, and commands. So are the `*.example.toml` files in `docs/examples/` and the text between `<!-- caudra-docgen:NAME -->` markers in other pages. Edit the source and run `make gen-docs`, never the output. For a new setting, the source is its metadata in `caudra-config`, which also feeds `caudra config example`. CI fails when the generated files are out of date.

Tests resolve every docs link and section anchor, so update the links when you rename a heading. [docs/AGENTS.md](docs/AGENTS.md) holds the voice, structure, and format rules. Source moves must preserve public `/docs/` URLs, example downloads, and section anchors.

The website is maintained separately and consumes these canonical sources. Its design, recordings, publishing, and JavaScript tooling belong to that repository. Native builds and offline docs read the checked-in sources directly.

## Working on Workcell

The file, shell, web, Python, and code-graph tools come from [Workcell](https://github.com/caudra/caudra/tree/main/workcell). Its crates live under `workcell/` as local members of this Rust workspace and share the root `Cargo.lock`.

Edit Workcell in this checkout and use the same Cargo and Make commands as for Caudra. For example, check the adapter after a tool change:

```sh
cargo nextest run -p caudra-workcell
```

Development and release builds use the same Workcell source. There are no sibling checkout overrides or separate revision bumps. Keep protocol-neutral tool behavior in `workcell/` and Caudra authorization, registration, and presentation in `caudra-workcell/`.

## Nix

CI checks the flake when the Nix files, the Rust or Lua sources, or the docs change. If you edit `flake.nix`, run the same checks locally:

```sh
nix build         # release build with the Python worker embedded
nix fmt           # format the Nix files
nix flake check   # includes the Git dependency hash check
```

## Releases

The first public release is `0.2.0-preview.1`, tagged `v0.2.0-preview.1`. Each preview gets a new numeric suffix. Release candidates may use `-rc.1` before the final `0.2.0`. Published tags and binaries are never replaced. During the `0.x` line, patches preserve compatibility and breaking changes require a minor bump with migration notes.

A release tag must match the workspace version exactly. Pushing it to the canonical repository authorizes CI to verify the exact commit, build all supported targets, stage a draft with installers and checksums, and publish automatically after every required check passes. Prereleases carry GitHub's pre-release flag and cannot become Latest. A failed job leaves publication incomplete rather than exposing a partial release. Ordinary pushes to `main` do not publish binaries.

Write reviewed release notes in `release-notes/<version>.md` before tagging. Describe user-visible changes, known limitations and migration steps. The pipeline reads this file from the tagged commit and adds the title, preview status and source link. Missing notes or unresolved placeholders stop validation. The same content is used for draft creation and publication, so manual draft edits are not the authoring workflow.

Set the GitHub Actions repository variable `CAUDRA_RUNNER_PROFILE` to select a reviewed runner mapping. An unset value defaults to `github`, using standard GitHub-hosted runners. `blacksmith` selects accelerated runners where supported. `ubicloud-blacksmith` selects Ubicloud for Linux and Blacksmith for Apple Silicon macOS and Windows. Intel macOS and lightweight control jobs remain on GitHub. The mappings live in `scripts/ci-runners.py`. Unknown profiles stop validation.

The hybrid profile requires the Ubicloud integration and account-level Premium Runners setting. Premium applies to x64. Ubicloud ARM runners use Ampere hardware, and workflow labels still use `ubicloud-standard-*`. Selecting a profile does not configure provider accounts or guarantee a particular delivered CPU. Changing the variable does not migrate running jobs.

`CAUDRA_EXPANDED_ATTRIBUTION=false` or `0` selects compact attribution, also the default when unset. The installed bundle keeps readable licenses and notices plus a compressed evidence/source companion. `true` or `1` selects the expanded audit tree. Both modes perform the same license validation. The selected mode is recorded in artifacts and fixed for the release attempt. Installers follow the verified artifact rather than querying repository variables.

Release archives include their license bundles and the embedded Python worker. Binary distribution clearance does not imply clearance to distribute every dependency source or build cache. Review the attribution evidence and unresolved exceptions before publication. Repository-level immutable releases and protected release tags are recommended operational settings, not guarantees made by the workflow.

The website deploys separately. Its pinned application source supplies canonical docs and bootstrap installers. Installer changes require a source-pin update after the source commit is available. Self-updates use the installer attached to the selected GitHub Release.

## Commits and pull requests

Branch from `main` and open your pull request against `main`. Keep each pull request to one change, so it is easy to review and to revert.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/) with a scope. Write the summary in the imperative and in lowercase, and use the body to explain why the change exists. A `!` marks a breaking change. Some examples from the history:

```text
feat(messaging): add consumer groups for topic work
fix(storage): keep SQLite's POSIX locks when opening caudra.db
docs(messaging): give cross-session messaging its own page
feat(ui)!: rename the default themes to caudra-dark and caudra-light
```

In the pull request, describe what changed and why, and link the issue. Say how you tested it. For a change to the terminal UI, a screenshot or a short recording helps a lot. If an AI agent wrote part of the change, say so and describe how you used it, with prompts where they help a reviewer follow the change.

CI runs on every pull request. It checks Rust and Lua formatting, runs clippy and the tests, checks the generated docs and unused dependencies, and builds on Linux, macOS, and Windows. Changes to the Python scripts or the Nix files start their own workflows.

## What reviewers look for

- Behavior that is explicit, and code that is easy to inspect.
- A small fix rather than a new abstraction.
- Tests for behavior and failure modes instead of implementation details.
- Non-obvious tradeoffs, explained in the pull request.
- Changes to permissions, stored data, network use, or compatibility, called out in the description and never hidden in a cleanup commit.

## License and attribution

New first-party contributions intentionally submitted for inclusion in Caudra are accepted under the [Apache License, Version 2.0](LICENSE), subject to its contribution terms. If you intend different terms, state them explicitly before submitting so the maintainers can review them.

This policy applies going forward. Maki-derived code and prior MIT-licensed Caudra contributions retain their [MIT terms](THIRD_PARTY_LICENSES/Maki.txt). It does not change historical license grants or assert an Apache patent grant from prior MIT contributors. [NOTICE.md](NOTICE.md) records the transition and where the code comes from.

If you adapt code from another project, retain its license and copyright and attribution notices, including file-level notices. Add the license text to `THIRD_PARTY_LICENSES/` and a note to `NOTICE.md`. Third-party MIT, Apache-2.0, and other applicable terms remain in force.
