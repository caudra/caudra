monty_worker_name := if os_family() == "windows" { "monty.exe" } else { "monty" }
monty_worker := justfile_directory() + "/target/code-worker/bin/" + monty_worker_name
monty_version := "0.0.21"
workcell_root := justfile_directory() + "/../workcell-mcp/crates"
cargo_cmd := justfile_directory() + "/scripts/dev-cargo.sh"
export WORKCELL_LOCAL := workcell_root + "/workcell"
export WORKCELL_BUNDLED_MONTY_WORKER := monty_worker

default:
    @just --list

# Build the pinned worker that Workcell embeds at compile time.
code-worker:
    if [ ! -x "{{ monty_worker }}" ] || ! "{{ monty_worker }}" --version 2>&1 | grep -qx "monty-runtime {{ monty_version }}"; then cargo install monty-runtime --version "={{ monty_version }}" --locked --no-default-features --force --root target/code-worker --target-dir target/code-worker-build; fi
    "{{ monty_worker }}" --version

# Install Caudra with the pinned worker embedded.
install: code-worker
    cargo install --locked --path . --force

build *ARGS:
    "{{ cargo_cmd }}" build {{ ARGS }}

# Types only, no codegen, no lints. Add `-p <crate>` to make it cheaper still.
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

fmt:
    cargo fmt --all
    stylua plugins/

pylint:
    ruff check scripts/
    ty check scripts/

gen-docs:
    "{{ cargo_cmd }}" run -p caudra-docgen

gen-docs-check:
    "{{ cargo_cmd }}" run -p caudra-docgen -- --check

machete:
    cargo machete

# Full CI check
ci: code-worker fmt-check lint pylint test gen-docs-check machete
