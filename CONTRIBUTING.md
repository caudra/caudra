# Contributing to Caudra

Thank you for helping with Caudra. Bug reports, fixes, documentation, and new features are all welcome. This guide shows how to set up a checkout, which checks to run, and what a pull request needs. If something in it is wrong or unclear, an issue about that helps too.

Caudra is an independent fork, developed at [github.com/caudra/caudra](https://github.com/caudra/caudra). Issues and pull requests go there.

Everyone who takes part in the project follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## Before you start

Search open and closed issues before you open a new one. Someone may already be working on it. Questions about setup or usage fit [Discussions Q&A](https://github.com/caudra/caudra/discussions/categories/q-a) better than an issue.

A good bug report names the version you run (`caudra --version`), your operating system and terminal, what you did, and what you expected instead. The issue forms ask for each of these. Logs help a lot, and `caudra logs` prints the latest records. Logs leave out prompt text, model output, and tool input. Provider error messages are kept in full, so read the log before you paste it into an issue.

Do not report a security vulnerability in a public issue. [SECURITY.md](SECURITY.md) explains how to report one privately.

For a larger change, open an issue first and describe your plan. That covers new features, new dependencies, and changes to permissions, stored data, or config formats. That way we can agree on the approach before you spend time on it.

## Set up a checkout

### With Nix

[Nix](https://nixos.org/download/) gives you everything in one step. `nix develop` opens a shell with the pinned Rust toolchain, rust-analyzer, and every tool the checks use. If you use [direnv](https://direnv.net), the `.envrc` in the repository loads that shell when you enter the directory.

### Without Nix

Install these yourself:

- [rustup](https://rustup.rs). `rust-toolchain.toml` pins Rust 1.99.1, and rustup fetches it with clippy and rustfmt the first time you build.
- GNU Make and Bash for the targets in the `Makefile`, and [cargo-nextest](https://nexte.st) for the tests.
- Python 3, which builds the worker behind the `code` tool, and [ripgrep](https://github.com/BurntSushi/ripgrep), which some tests call.
- [stylua](https://github.com/JohnnyMorganz/StyLua), [ruff](https://docs.astral.sh/ruff/), [ty](https://github.com/astral-sh/ty), and [cargo-machete](https://github.com/bnjbvr/cargo-machete) for the formatting and lint checks.

On Windows, run Make from Git Bash and install GNU Make separately. CI provisions GNU Make explicitly. The Makefile uses `python3` by default. If your Python 3 executable is named `python`, use `make PYTHON=python <target>`.

Build logic stays in Cargo and Python. Application development, native builds, and docs generation do not require Bun, Node.js, or a website checkout.

### First build

```sh
git clone https://github.com/caudra/caudra
cd caudra
make run           # start a debug build
```

Cargo-compiling targets depend on `code-worker` automatically: `build`, `check`, `run`, `test`, `lint`, `install`, `install-fast`, `gen-docs`, `gen-docs-check`, and the Workcell targets that compile Rust. No manual bootstrap is needed. The first run downloads and compiles the worker, and later runs reuse it. Run `make code-worker` for an optional warmup. A plain `cargo build` works without the worker, but in that binary the `code` tool cannot run, and it says so at startup.

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

## MCP server tests

`make test-mcp-servers` runs the `code` tool against real MCP servers: server-everything, Executor, and Home Assistant, each pinned in a Dockerfile under `tests/mcp-servers/`. It needs Docker, and a remote daemon works through `DOCKER_HOST`. CI never runs these tests, and `make test` skips them. `scripts/mcp-servers.py` builds the images, starts and onboards Home Assistant, runs the tests, and then removes every container labelled `caudra-mcp-it`.

The tests in `caudra-workcell/tests/mcp_servers/` run fixed snippets through the `code` gateway. The ones in `tests/mcp_sessions/` drive the `caudra` binary over stream-json, as an SDK client would, and answer its permission prompts. Their model answers come from cassettes in `tests/mcp_sessions/cassettes/`, so they need no model and no credentials. When a change alters what Caudra asks the model, record them again with a real model:

```sh
CAUDRA_IT_RECORD=openai/gpt-6-luna make test-mcp-servers ARGS='-E "binary(mcp_sessions)"'
```

A recording uses the login a debug build saved, and copies it only while it has at least 30 minutes left, because a copy that refreshed it would sign the original out. Only a test that passes replaces its cassettes. Underneath, `CAUDRA_PROVIDER_RECORD=<file>` and `CAUDRA_PROVIDER_REPLAY=<file>` make any Caudra process record or replay its model answers.

To work on one test, start the servers once with `eval "$(python3 scripts/mcp-servers.py up)"`, run `cargo nextest run` with `--run-ignored only`, and remove them with `python3 scripts/mcp-servers.py down`.

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

## Keeping the model list current

The built-in model tables in `caudra-providers/src/providers/` hold each provider's ids, prices, limits, and defaults. `make model-watch` compares them with the [models.dev](https://models.dev) catalog and prints a Markdown report: models that no table names (those released in the last 90 days as new, older ones in a section of their own until each is added or accepted), entries past the shutdown day their table schedules, deprecations, changed prices and limits, table models the catalog does not list, and stale acceptances. Pass `ARGS='-- --since 2026-07-01'` to choose the start date, or `--catalog FILE` to read a saved copy of `api.json`. The prices of decision models, such as TypeSafe Jev and OpenAI's Decisions API, live in `caudra-providers/src/decision_pricing.rs`. models.dev does not list them, so every report restates that table with each entry's source page, to be checked by hand.

models.dev is a lead, not a source. Confirm every change on the provider's own model, pricing, or deprecation page. A divergence that is deliberate goes into `ACCEPTED` in `caudra-providers/src/model_watch.rs`, and a limit a table caps on purpose goes into `CAPS`, each with its reason. The `model-updates` skill in `.caudra/skills/` lists the sources, the rules for applying a change, and what each table needs.

The `model-watch` automation in `.caudra/automations/` runs that routine once a day. It merges `main` into its own branch, surveys, applies confirmed changes with the skill, validates, and then fast-forwards your local `main`. It waits while the main checkout is busy, and it never pushes. To set it up:

1. In your global `caudra.toml`, set `[autopilot] max_continuations = 0`.
2. Create its worktree: `git worktree add ../caudra-model-watch -b model-watch main`.
3. In that worktree, start `caudra --autopilot` in Build mode, trust the project's automations in `/automations`, and arm it with the path of your main checkout: `/automations arm model-watch {"main_worktree": "/path/to/caudra"}`.

Each run writes its report to `target/model-watch/<date>.md` in the worktree and notifies you when it lands, or when something needs a person.

## Keeping the skill packs current

Caudra bundles two skill packs: pstack in `caudra-pstack/`, from [cursor/plugins](https://github.com/cursor/plugins), and mattpocock in `caudra-mattpocock/`, from [mattpocock/skills](https://github.com/mattpocock/skills). Each crate's `upstream/` is a verbatim copy of the files Caudra bundles, chosen by its `pack.json`. Never edit it by hand: `make sync-pstack ARGS='--source <cursor/plugins checkout>'` and `make sync-mattpocock ARGS='--source <mattpocock/skills checkout>'` rewrite it, both through `scripts/sync-skillpack.py`. The packs are written for Cursor and Claude Code, so every file that names another agent's tool, path, model, command, or feature has a Caudra translation in the crate's `translated/`, which replaces the upstream page. The agent reads the translated pages as written and never maps names itself.

`translated/translations.json` records the upstream hash each translation was made from. After a sync, the script prints a line for each file that needs work: `TRANSLATE` when upstream changed a translated file, `NEW` when it added or changed one with no translation, `ORPHAN` when a translated file went away, and, for pstack, `PORT` when a script with a Rust port in `caudra-pstack/src/scripts/` changed. `cargo test -p caudra-pstack` and `cargo test -p caudra-mattpocock` fail while any page still names a foreign term or any translation is stale. The `skillpack-translate` skill in `.caudra/skills/` holds the rules for translating, with a reference page per pack, and `make sync-<pack> ARGS='--mark <path>...'` records a finished translation.

The `pstack-sync` automation in `.caudra/automations/` runs that routine once a day: it merges `main` into its own branch, pulls your cursor/plugins clone, syncs, retranslates with the skill, validates, and fast-forwards your local `main`. Set it up as `model-watch` above, in its own worktree on a `pstack-sync` branch, and arm it with both paths: `/automations arm pstack-sync {"main_worktree": "/path/to/caudra", "plugins_checkout": "/path/to/cursor-plugins"}`. Each run writes its report to `target/pstack-sync/<date>.md` in the worktree.

## Keeping a worktree current

The experimental `sync-main` automation in `.caudra/automations/` checks local `main` every 15 minutes, when armed, and when the session becomes idle. It spends no model turn while the worktree is current. When `main` moves, it asks an idle session to sync or sends a working session one request to add `Sync with main` after its current todo. The sync policy reads what arrived, adopts work that `main` already implements, merges at a safe point, and runs `make check`. It never pushes or changes the main checkout.

1. Enable `[experimental] automations = true` in your global `caudra.toml` and restart Caudra.
2. If this worktree branched before the script landed, run one manual `git merge main` to get it.
3. Trust `sync-main` once in `/automations`. That grant carries to sibling worktrees with the same script.
4. In each worktree session, run `/automations arm sync-main`.

The optional args are `base` (default `"main"`), `check` (`"make check"`), `brief_over` (`5` commits), and `notice_hours` (`12` hours). A blocked sync notifies once per base SHA and waits until the base moves or you arm it again manually. Editing the script, including receiving an edit through a merge, revokes its trust until you approve the new digest.

## Working on Workcell

The file, shell, web, Python, and source-code tools come from [Workcell](https://github.com/caudra/caudra/tree/main/workcell). Its crates live under `workcell/` as local members of this Rust workspace and share the root `Cargo.lock`.

Edit Workcell in this checkout and use the same Cargo and Make commands as for Caudra. For example, check the adapter after a tool change:

```sh
cargo nextest run -p caudra-workcell
```

Development and release builds use the same Workcell source. There are no sibling checkout overrides or separate revision bumps. Keep protocol-neutral tool behavior in `workcell/` and Caudra authorization, registration, and presentation in `caudra-workcell/`.

## Nix

The flake supports x86_64 and aarch64 Linux, and Apple Silicon macOS. Intel macOS uses the native build because current nixpkgs no longer supports it.

CI checks the flake when the Nix files, the Rust or Lua sources, or the docs change. If you edit `flake.nix`, run the same checks locally:

```sh
nix build         # release build with the Python worker embedded
nix fmt           # format the Nix files
nix flake check   # includes the Git dependency hash check
```

## Releases

The first public release is `0.2.0-preview.1`, tagged `v0.2.0-preview.1`. Each preview gets a new numeric suffix. Release candidates may use `-rc.1` before the final `0.2.0`. Published tags and binaries are never replaced. Versions follow the rules in [Versions and persisted formats](#versions-and-persisted-formats).

A release tag must match the workspace version exactly. Pushing it to the canonical repository authorizes CI to verify the exact commit, build all supported targets, stage a draft with installers and checksums, and publish automatically after every required check passes. Prereleases carry GitHub's pre-release flag and cannot become Latest. A failed job leaves publication incomplete rather than exposing a partial release. Ordinary pushes to `main` do not publish binaries.

A tag needs reviewed notes in `release-notes/<version>.md`, written as described in [Release notes](#release-notes).

Set the GitHub Actions repository variable `CAUDRA_RUNNER_PROFILE` to select a reviewed runner mapping. An unset value defaults to `github`, using standard GitHub-hosted runners. `blacksmith` selects accelerated runners where supported. `ubicloud-blacksmith` selects Ubicloud for Linux and Blacksmith for Apple Silicon macOS and Windows. Intel macOS and lightweight control jobs remain on GitHub. The mappings live in `scripts/ci-runners.py`. Unknown profiles stop validation.

The hybrid profile requires the Ubicloud integration and account-level Premium Runners setting. Premium applies to x64. Ubicloud ARM runners use Ampere hardware, and workflow labels still use `ubicloud-standard-*`. Selecting a profile does not configure provider accounts or guarantee a particular delivered CPU. Changing the variable does not migrate running jobs.

`CAUDRA_EXPANDED_ATTRIBUTION=false` or `0` selects compact attribution, also the default when unset. The installed bundle keeps readable licenses and notices plus a compressed evidence/source companion. `true` or `1` selects the expanded audit tree. Both modes perform the same license validation. The selected mode is recorded in artifacts and fixed for the release attempt. Installers follow the verified artifact rather than querying repository variables.

Release archives include their license bundles and the embedded Python worker. Binary distribution clearance does not imply clearance to distribute every dependency source or build cache. Review the attribution evidence and unresolved exceptions before publication. Repository-level immutable releases and protected release tags are recommended operational settings, not guarantees made by the workflow.

The website deploys separately. Verified development commits propose a `docs-source.json` update in the website repository. The GitHub App merges only its unchanged, pin-only PR after the canonical website checks pass. Website `main` deploys to a persistent preview, while pull requests retain their own previews. This flow does not require GitHub's native auto-merge feature or bypass branch protections.

After the complete release workflow succeeds, the source automation dispatches the published release identity to the website repository. The website independently verifies the release, successful source run, and exact tag commit before building production inputs. Production uses that release commit, not a newer development pin. Drafts, failed releases, moved tags, and stale automatic release requests cannot promote a preview to production. Self-updates use the installer attached to the selected GitHub Release.

Configure `WEBSITE_APP_ID` and `WEBSITE_APP_PRIVATE_KEY` in the application repository. Install the App only on the website repository with Contents and Pull requests write permissions and Actions read permission. The latter lets the bot verify private website CI before merging. Deploy the website workflow changes before enabling the source automation changes so development pin updates cannot reach the old production-on-main path.

### Versions and persisted formats

Caudra follows SemVer, and the version tells users whether an update can be undone:

- A **patch** (`X.Y.Z`) holds fixes only. It never changes a persisted format: the session database, config files, journals, credential and other stored records, or the protocol between running Caudra processes. Users can update and run `caudra rollback` freely within `X.Y`, and different patches can run side by side.
- A **minor** (`X.Y.0`) holds features and may migrate data one way. Its notes carry the upgrade alert described in [Release notes](#release-notes).
- A **major** release, after 1.0, means the user must act.
- **Prereleases** of `X.Y.0` may migrate between each other. Returning from one to the last stable release means restoring a backup from `backups/`.

`main` always works toward the next minor, as `X.Y.0-preview.N`. Patches come from `release/X.Y`, cut from the `vX.Y.0` tag. A fix lands on `main` first and is copied with `git cherry-pick -x`. After each patch tag, merge `release/X.Y` back into `main` so its release notes come along. On a `Cargo.toml` version conflict, keep the higher version from `main`.

`scripts/persisted-formats.toml` lists every integer `const` whose name holds `VERSION` or `FORMAT`, outside tests and `vendor/`. Each one is either tracked as a persisted format or exempt with a reason. `make formats-lint`, also run by CI, fails on a const that is missing from the list. When you add one, track it if it versions anything Caudra writes to disk or sends to another Caudra process, and exempt it otherwise. `make formats-check` compares the tracked values with `vX.Y.0` and fails when a patch changes, adds or removes one. It runs on pushes and pull requests to `release/**` and when a release tag is validated, and it skips prereleases of `X.Y.0`.

The check only sees version constants. A patch must also not write anything a `vX.Y.0` binary cannot read, such as a new serde field or enum variant that an older build rejects. Reviewers check this by hand.

### Release notes

Each release's notes live in `release-notes/<version>.md`. The same file is the GitHub release body, what Caudra shows after an update and in `/changelog`, and a section of the [Release notes](https://caudra.ai/docs/release-notes/) docs page, so it holds only what changed. The page is never checked in: Caudra builds it from the files for `/docs` and the `caudra-docs` skill, and the website builds its own, so branches that each add notes only conflict in the files they share.

Notes are written as changes land, not at release time. The draft is the newest file in `release-notes/` whose version has no tag yet. After a release, the first user-visible change starts the next file, such as `0.2.0-preview.7.md` after `v0.2.0-preview.6`. The workspace version is bumped only when the release is prepared, so the draft is usually ahead of it, and builds leave it out of `/changelog` until then. The Rust tests allow at most one file ahead of the workspace version.

When you draft:

- Update the draft in the same commit as the change. Leave out refactors, tests, CI and internal tooling.
- Write for someone updating: what they will notice and what they must do, not how it was built.
- Fold a follow-up into the bullet it refines, and remove the bullet of a reverted change.
- Keep the highlights in step as the release grows, so they still sum up the whole release.
- Size each bullet to its change. A headline change gets a full bullet. A small fix or a cosmetic change gets one sentence, or a clause in the bullet it belongs to. Before you add a bullet, look for one about the same work and extend it.
- Leave out how it was built and what a user does not notice: internal names, algorithms, every edge case, and the reasoning behind a fix. The docs link carries the details.
- Write in the voice of [`docs/AGENTS.md`](docs/AGENTS.md): no em dashes, semicolons, or contractions outside code.

Every file follows the same format:

1. Start with a bullet list of the release's highlights: one plain line per headline change, no bold lead-in, about three to eight items. Nothing comes before it.
2. Put anything a user must do in a `> [!IMPORTANT]`, `[!WARNING]`, `[!CAUTION]`, `[!NOTE]` or `[!TIP]` alert, and say when it applies, such as "Before you update" or "After you update, restart". A release that upgrades the session database says so in an alert like the one in `0.2.0-preview.6.md`.
3. Group changes under `###` headings, never `#` or `##`.
4. Start each bullet with a bold lead-in that names the change.
5. Leave out install and update steps. The pipeline adds a "New to Caudra?" section with the [Quick Start](https://caudra.ai/docs/quick-start/), and an "Updating from" section that names the previous release and the update command for its channel.

The Rust tests also hold every file to these limits: at most 8 highlights of up to 35 words each, bullets of up to 180 words, and 3,000 words for the whole release. The test names each bullet over a limit.

The pipeline reads the file from the tagged commit and adds a status line. GitHub shows the title, tag and commit beside the notes, so the notes do not repeat them. Missing notes, unresolved placeholders, and notes that break these rules stop validation, and the Rust tests check every file in `release-notes/` the same way. The same content is used for draft creation and publication, so manual draft edits are not the authoring workflow.

The website reads `release-notes/*.md` from the commit it pins, alongside `docs/content/`, and relies on this contract:

- One file per release, named `<version>.md` with a SemVer version and no `v` prefix. Order releases by SemVer precedence, newest first.
- A file holds the body only, with no title. The title comes from the version, as `Caudra 0.2 Preview 4` for `0.2.0-preview.4`, and the tag is `v<version>`.
- Alerts use GitHub's `> [!KIND]` syntax, and links into the docs are absolute `https://caudra.ai/docs/` URLs. The Rust tests check that every docs link resolves.
- The newest file may be a draft for a release that has no tag yet. There is at most one.

## Commits and pull requests

Branch from `main` and open your pull request against `main`. Keep each pull request to one change, so it is easy to review and to revert.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/) with a scope. Write the summary in the imperative and in lowercase, and use the body to explain why the change exists. A `!` marks a breaking change. Some examples from the history:

```text
feat(messaging): add consumer groups for topic work
fix(storage): keep SQLite's POSIX locks when opening caudra.db
docs(messaging): give cross-session messaging its own page
feat(ui)!: rename the default themes to caudra-dark and caudra-light
```

In the pull request, describe what changed and why, and link the issue. Say how you tested it. For a change to the terminal UI, a screenshot or a short recording helps a lot. If an AI agent wrote part of the change, say so and describe how you used it, with prompts where they help a reviewer follow the change. The pull request template has a section for each of these.

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
