monty_worker_name := if os_family() == "windows" { "monty.exe" } else { "monty" }
monty_worker := justfile_directory() + "/target/code-worker/bin/" + monty_worker_name
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
    cargo build {{ ARGS }}

# Types only, no codegen, no lints, always for the whole workspace. For one crate: `cargo check -p <crate> --tests`.
check *ARGS:
    cargo check --workspace --tests {{ ARGS }}

run *ARGS:
    cargo run --package caudra {{ ARGS }}

test *ARGS:
    WORKCELL_REQUIRE_CODE_WORKER=1 cargo nextest run --workspace {{ ARGS }}

lint:
    cargo clippy --all --tests -- -D warnings

lint-fix:
    cargo clippy --all --tests --fix

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

distribution-tests:
    python3 scripts/test-build-code-worker.py
    python3 scripts/test-build-attribution.py
    python3 scripts/test-install.py

gen-docs:
    cargo run -p caudra-docgen

gen-docs-check:
    cargo run -p caudra-docgen -- --check

workcell-build *ARGS:
    cargo build --locked --package workcell-mcp {{ ARGS }}

workcell-run *ARGS: code-worker
    cargo run --locked --package workcell-mcp -- {{ ARGS }}

workcell-release: code-worker
    cargo build --locked --release --package workcell-mcp

workcell-check-native:
    make -C workcell check-native

workcell-doc-test:
    cargo test --locked --doc --package 'workcell*'

machete:
    cargo machete

# Full CI check
ci: code-worker fmt-check lint pylint distribution-tests workcell-check-native test workcell-doc-test gen-docs-check machete
