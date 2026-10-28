SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

CARGO ?= cargo
PYTHON ?= python3
ARGS ?=

include scripts/cargo-cache.mk

ifeq ($(OS),Windows_NT)
MONTY_WORKER_NAME := monty.exe
else
MONTY_WORKER_NAME := monty
endif

export WORKCELL_BUNDLED_MONTY_WORKER := $(CURDIR)/target/code-worker/bin/$(MONTY_WORKER_NAME)

.PHONY: help code-worker incremental-sweep install install-fast build check run test test-mcp-servers lint lint-fix fmt-check fmt pylint distribution-tests formats-lint formats-check gen-docs gen-docs-check model-watch sync-pstack sync-mattpocock workcell-build workcell-run workcell-release workcell-check-native workcell-doc-test machete hakari ci

help:
	@printf '%s\n' \
		'Caudra development targets:' \
		'  make build          Build Caudra with the pinned worker' \
		'  make run            Run Caudra (Cargo flags via ARGS)' \
		'  make install        Install Caudra with the pinned worker' \
		'  make install-fast   Install using the release-fast profile' \
		'  make code-worker    Prepare the worker without building Caudra' \
		'  make incremental-sweep  Trim incremental caches to INCREMENTAL_CACHE_SIZE (default 10G)' \
		'  make check          Type-check workspace tests' \
		'  make lint           Run workspace Clippy with warnings denied' \
		'  make lint-fix       Apply Clippy fixes' \
		'  make test           Run workspace tests with nextest' \
		'  make test-mcp-servers  Run the MCP server tests against pinned servers in Docker' \
		'  make fmt            Format Rust, Lua, and Python' \
		'  make fmt-check      Check Rust, Lua, and Python formatting' \
		'  make pylint         Check Python lint and types' \
		'  make distribution-tests  Test build and distribution helpers' \
		'  make formats-lint   Check every versioned const is listed in scripts/persisted-formats.toml' \
		'  make formats-check  Check a patch keeps the persisted formats of its vX.Y.0' \
		'  make gen-docs       Regenerate canonical documentation' \
		'  make gen-docs-check Check generated documentation' \
		'  make model-watch    Report model changes from models.dev (flags via ARGS="-- --since DATE")' \
		'  make sync-pstack    Refresh the bundled pstack (ARGS="--source <cursor/plugins checkout>")' \
		'  make sync-mattpocock  Refresh the bundled mattpocock skills (ARGS="--source <mattpocock/skills checkout>")' \
		'  make workcell-build Build the standalone Workcell server' \
		'  make workcell-run   Run Workcell (server flags via ARGS)' \
		'  make workcell-release  Build the release Workcell server' \
		'  make workcell-check-native  Check native Workcell features' \
		'  make workcell-doc-test  Run Workcell doctests' \
		'  make machete        Check unused dependencies' \
		'  make hakari         Check the workspace-hack is current' \
		'  make ci             Run local application CI checks' \
		'Cargo flags: make build ARGS="--release"' \
		'Program arguments: make run ARGS="-- auth login"' \
		'Compiler cache: Cargo runs through sccache when installed; opt out with SCCACHE='

code-worker:
	$(PYTHON) scripts/build-code-worker.py

incremental-sweep:
	$(PYTHON) scripts/sweep-incremental.py

install: code-worker incremental-sweep
	$(CARGO) install --locked --path . --force

install-fast: code-worker incremental-sweep
	$(CARGO) install --locked --path . --force --profile release-fast

build: code-worker incremental-sweep
	$(CARGO) build $(ARGS)

check: code-worker incremental-sweep
	$(CARGO) check --workspace --tests $(ARGS)

run: code-worker incremental-sweep
	$(CARGO) run --package caudra $(ARGS)

test: code-worker incremental-sweep
	WORKCELL_REQUIRE_CODE_WORKER=1 $(CARGO) nextest run --workspace $(ARGS)

test-mcp-servers: export CARGO := $(CARGO)
test-mcp-servers: code-worker incremental-sweep
	WORKCELL_REQUIRE_CODE_WORKER=1 $(PYTHON) scripts/mcp-servers.py test $(ARGS)

lint: code-worker incremental-sweep
	$(CARGO) clippy --all --tests -- -D warnings

lint-fix: code-worker incremental-sweep
	$(CARGO) clippy --all --tests --fix

fmt-check:
	$(CARGO) fmt --all -- --check
	stylua --check plugins/
	ruff format --check scripts/

fmt:
	$(CARGO) fmt --all
	stylua plugins/
	ruff format scripts/

pylint:
	ruff check scripts/
	ty check scripts/

distribution-tests:
	$(PYTHON) scripts/test-ci-runners.py
	$(PYTHON) scripts/test-sweep-incremental.py
	$(PYTHON) scripts/test-time-command.py
	$(PYTHON) scripts/test-make.py
	$(PYTHON) scripts/test-build-code-worker.py
	$(PYTHON) scripts/test-build-attribution.py
	$(PYTHON) scripts/test-install.py
	$(PYTHON) scripts/test-release.py
	$(PYTHON) scripts/test-persisted-formats.py

formats-lint:
	$(PYTHON) scripts/persisted-formats.py lint

formats-check:
	$(PYTHON) scripts/persisted-formats.py check

gen-docs: code-worker incremental-sweep
	$(CARGO) run -p caudra-docgen

gen-docs-check: code-worker incremental-sweep
	$(CARGO) run -p caudra-docgen -- --check

model-watch: code-worker incremental-sweep
	$(CARGO) run -q -p caudra-docgen --bin caudra-model-watch $(ARGS)

sync-pstack:
	$(PYTHON) scripts/sync-skillpack.py pstack $(ARGS)

sync-mattpocock:
	$(PYTHON) scripts/sync-skillpack.py mattpocock $(ARGS)

workcell-build: code-worker incremental-sweep
	$(CARGO) build $(if $(filter --locked,$(ARGS)),,--locked) --package workcell-mcp $(ARGS)

workcell-run: code-worker incremental-sweep
	$(CARGO) run --locked --package workcell-mcp -- $(ARGS)

workcell-release: code-worker incremental-sweep
	$(CARGO) build --locked --release --package workcell-mcp

workcell-check-native: code-worker incremental-sweep
	$(MAKE) -C workcell check-native

workcell-doc-test: code-worker incremental-sweep
	$(CARGO) test --locked --doc --package 'workcell*'

machete:
	$(CARGO) machete

hakari:
	$(CARGO) hakari generate --diff
	$(CARGO) hakari manage-deps --dry-run

ci: fmt-check lint pylint formats-lint distribution-tests workcell-check-native test workcell-doc-test gen-docs-check machete hakari
