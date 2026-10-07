SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

CARGO ?= cargo
PYTHON ?= python3
ARGS ?=

ifeq ($(OS),Windows_NT)
MONTY_WORKER_NAME := monty.exe
else
MONTY_WORKER_NAME := monty
endif

export WORKCELL_BUNDLED_MONTY_WORKER := $(CURDIR)/target/code-worker/bin/$(MONTY_WORKER_NAME)

.PHONY: help code-worker install install-fast build check run test lint lint-fix fmt-check fmt pylint distribution-tests gen-docs gen-docs-check workcell-build workcell-run workcell-release workcell-check-native workcell-doc-test machete ci

help:
	@printf '%s\n' \
		'Caudra development targets:' \
		'  make build          Build Caudra with the pinned worker' \
		'  make run            Run Caudra (Cargo flags via ARGS)' \
		'  make install        Install Caudra with the pinned worker' \
		'  make install-fast   Install using the release-fast profile' \
		'  make code-worker    Prepare the worker without building Caudra' \
		'  make check          Type-check workspace tests' \
		'  make lint           Run workspace Clippy with warnings denied' \
		'  make lint-fix       Apply Clippy fixes' \
		'  make test           Run workspace tests with nextest' \
		'  make fmt            Format Rust, Lua, and Python' \
		'  make fmt-check      Check Rust, Lua, and Python formatting' \
		'  make pylint         Check Python lint and types' \
		'  make distribution-tests  Test build and distribution helpers' \
		'  make gen-docs       Regenerate canonical documentation' \
		'  make gen-docs-check Check generated documentation' \
		'  make workcell-build Build the standalone Workcell server' \
		'  make workcell-run   Run Workcell (server flags via ARGS)' \
		'  make workcell-release  Build the release Workcell server' \
		'  make workcell-check-native  Check native Workcell features' \
		'  make workcell-doc-test  Run Workcell doctests' \
		'  make machete        Check unused dependencies' \
		'  make ci             Run local application CI checks' \
		'Cargo flags: make build ARGS="--release"' \
		'Program arguments: make run ARGS="-- auth login"'

code-worker:
	$(PYTHON) scripts/build-code-worker.py

install: code-worker
	$(CARGO) install --locked --path . --force

install-fast: code-worker
	$(CARGO) install --locked --path . --force --profile release-fast

build: code-worker
	$(CARGO) build $(ARGS)

check: code-worker
	$(CARGO) check --workspace --tests $(ARGS)

run: code-worker
	$(CARGO) run --package caudra $(ARGS)

test: code-worker
	WORKCELL_REQUIRE_CODE_WORKER=1 $(CARGO) nextest run --workspace $(ARGS)

lint: code-worker
	$(CARGO) clippy --all --tests -- -D warnings

lint-fix: code-worker
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
	$(PYTHON) scripts/test-make.py
	$(PYTHON) scripts/test-build-code-worker.py
	$(PYTHON) scripts/test-build-attribution.py
	$(PYTHON) scripts/test-install.py
	$(PYTHON) scripts/test-release.py

gen-docs: code-worker
	$(CARGO) run -p caudra-docgen

gen-docs-check: code-worker
	$(CARGO) run -p caudra-docgen -- --check

workcell-build: code-worker
	$(CARGO) build --locked --package workcell-mcp $(ARGS)

workcell-run: code-worker
	$(CARGO) run --locked --package workcell-mcp -- $(ARGS)

workcell-release: code-worker
	$(CARGO) build --locked --release --package workcell-mcp

workcell-check-native: code-worker
	$(MAKE) -C workcell check-native

workcell-doc-test: code-worker
	$(CARGO) test --locked --doc --package 'workcell*'

machete:
	$(CARGO) machete

ci: fmt-check lint pylint distribution-tests workcell-check-native test workcell-doc-test gen-docs-check machete
