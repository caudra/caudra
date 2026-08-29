monty_worker_name := if os_family() == "windows" { "monty.exe" } else { "monty" }
monty_worker := justfile_directory() + "/target/code-worker/bin/" + monty_worker_name
export MAKI_MONTY_WORKER := monty_worker

default:
    @just --list

# Build the pinned worker that maki-workcell embeds at compile time.
code-worker:
    cargo install monty-runtime --version "=0.0.21" --locked --no-default-features --root target/code-worker --target-dir target/code-worker-build
    "{{monty_worker}}" --version

build *ARGS:
    cargo build {{ARGS}}

# Types only, no codegen, no lints. Add `-p <crate>` to make it cheaper still.
check *ARGS:
    cargo check --workspace --tests {{ARGS}}

run *ARGS:
    cargo run {{ARGS}}

test *ARGS:
    cargo nextest run --workspace {{ARGS}}

lint:
    cargo clippy --all --tests -- -D warnings

lint-fix:
    cargo clippy --all --tests --fix

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
    cargo run -p maki-docgen

gen-docs-check:
    cargo run -p maki-docgen -- --check

machete:
    cargo machete

# Full CI check
ci: code-worker fmt-check lint pylint test gen-docs-check machete
