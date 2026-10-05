monty_worker_name := if os_family() == "windows" { "monty.exe" } else { "monty" }
monty_worker := justfile_directory() + "/target/code-worker/bin/" + monty_worker_name
workcell_root := justfile_directory() + "/../workcell-mcp/crates"
cargo_cmd := justfile_directory() + "/scripts/dev-cargo.sh"
export WORKCELL_LOCAL := workcell_root + "/workcell"
export WORKCELL_BUNDLED_MONTY_WORKER := monty_worker

default:
    @just --list

# Build the pinned worker that Workcell embeds at compile time.
code-worker:
    python3 scripts/build-code-worker.py

# Install Caudra with the pinned worker embedded.
install: code-worker
    cargo install --locked --path . --force

# Install a release-fast build for local release testing: no LTO, parallel and incremental codegen.
install-fast: code-worker
    cargo install --locked --path . --force --profile release-fast

build *ARGS:
    "{{ cargo_cmd }}" build {{ ARGS }}

# Types only, no codegen, no lints, always for the whole workspace. For one crate: `cargo check -p <crate> --tests`.
check *ARGS:
    "{{ cargo_cmd }}" check --workspace --tests {{ ARGS }}

run *ARGS:
    "{{ cargo_cmd }}" run {{ ARGS }}

test *ARGS:
    "{{ cargo_cmd }}" nextest run --workspace {{ ARGS }}

lint:
    "{{ cargo_cmd }}" clippy --all --tests -- -D warnings

lint-fix:
    "{{ cargo_cmd }}" clippy --all --tests --fix

fmt-check:
    cargo fmt --all -- --check
    stylua --check plugins/
    ruff format --check scripts/

fmt:
    cargo fmt --all
    stylua plugins/
    ruff format scripts/

pylint:
    ruff check scripts/
    ty check scripts/

gen-docs:
    "{{ cargo_cmd }}" run -p caudra-docgen

gen-docs-check:
    "{{ cargo_cmd }}" run -p caudra-docgen -- --check

site-install:
    bun install --cwd "{{ justfile_directory() }}/site" --frozen-lockfile

site-dev *ARGS:
    bun run --cwd "{{ justfile_directory() }}/site" dev {{ ARGS }}

site-check:
    bun run --cwd "{{ justfile_directory() }}/site" check

site-test:
    bun run --cwd "{{ justfile_directory() }}/site" test

site-build:
    bun run --cwd "{{ justfile_directory() }}/site" build

site-output:
    bun run --cwd "{{ justfile_directory() }}/site" test:output

site-browser:
    bun run --cwd "{{ justfile_directory() }}/site" test:browser

site-preview *ARGS:
    bun run --cwd "{{ justfile_directory() }}/site" preview {{ ARGS }}

machete:
    cargo machete

# Pin Workcell at its latest pushed commit and refresh the flake's dependency hashes.
bump-workcell *ARGS:
    scripts/bump-workcell.py {{ ARGS }}

# Full CI check
ci: code-worker fmt-check lint pylint test gen-docs-check machete site-install site-check site-test site-build site-output
