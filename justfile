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

machete:
    cargo machete

# Pin Workcell at its latest pushed commit and refresh the flake's dependency hashes.
bump-workcell *ARGS:
    scripts/bump-workcell.py {{ ARGS }}

# Full CI check
ci: code-worker fmt-check lint pylint test gen-docs-check machete
