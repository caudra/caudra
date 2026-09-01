monty_worker_name := if os_family() == "windows" { "monty.exe" } else { "monty" }
monty_worker := justfile_directory() + "/target/code-worker/bin/" + monty_worker_name
monty_version := "0.0.21"
workcell_root := justfile_directory() + "/../workcell-mcp/crates"
# Preserve the Git-pinned lockfile while resolving the sibling Workcell workspace locally.
local_workcell := if shell("test -f '" + workcell_root + "/workcell/Cargo.toml' && printf yes || true") == "yes" {
    "--config 'paths=[\"" + workcell_root + "/workcell\"]'"
} else {
    ""
}
export WORKCELL_BUNDLED_MONTY_WORKER := monty_worker

default:
    @just --list

# Build the pinned worker that Workcell embeds at compile time.
code-worker:
    if [ ! -x "{{ monty_worker }}" ] || ! "{{ monty_worker }}" --version 2>&1 | grep -qx "monty-runtime {{ monty_version }}"; then cargo install monty-runtime --version "={{ monty_version }}" --locked --no-default-features --force --root target/code-worker --target-dir target/code-worker-build; fi
    "{{ monty_worker }}" --version

build *ARGS:
    cargo {{local_workcell}} build {{ARGS}}

# Types only, no codegen, no lints. Add `-p <crate>` to make it cheaper still.
check *ARGS:
    cargo {{local_workcell}} check --workspace --tests {{ARGS}}

run *ARGS:
    cargo {{local_workcell}} run {{ARGS}}

test *ARGS:
    cargo nextest run {{local_workcell}} --workspace {{ARGS}}

lint:
    cargo clippy {{local_workcell}} --all --tests -- -D warnings

lint-fix:
    cargo clippy {{local_workcell}} --all --tests --fix

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
    cargo {{local_workcell}} run -p caudra-docgen

gen-docs-check:
    cargo {{local_workcell}} run -p caudra-docgen -- --check

machete:
    cargo machete

# Full CI check
ci: code-worker fmt-check lint pylint test gen-docs-check machete
