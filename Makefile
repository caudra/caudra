SHELL := /bin/bash
.DEFAULT_GOAL := ci

CARGO ?= cargo
DOCKER ?= docker
IMAGE ?= workcell-mcp
TAG ?= local
ROOT ?=
PORT ?= 3001
ARGS ?=

.PHONY: help fmt fmt-check check clippy test build release ci install run run-web clean docker-build docker-smoke docker-run

help:
	@printf '%s\n' \
		'Workcell MCP targets:' \
		'  make fmt           Format the Rust workspace' \
		'  make fmt-check     Verify formatting without changing files' \
		'  make check         Type-check all workspace targets' \
		'  make clippy        Run Clippy with warnings denied' \
		'  make test          Run all workspace tests with the lockfile' \
		'  make build         Build the debug workspace with the lockfile' \
		'  make release       Build the optimized binary with the lockfile' \
		'  make ci            Run the complete local CI verification' \
		'  make install       Install workcell-mcp from this checkout' \
		'  make run ROOT=...  Run all tool groups over stdio' \
		'  make run-web       Run only web tools over stdio' \
		'  make docker-build  Build IMAGE:TAG (defaults to workcell-mcp:local)' \
		'  make docker-smoke  Verify the built container starts' \
		'  make docker-run ROOT=... WORKCELL_MCP_HTTP_TOKEN=...  Run hardened HTTP' \
		'  make clean         Remove Cargo build artifacts'

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all --check

check:
	$(CARGO) check --workspace --all-targets --locked

clippy:
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

test:
	$(CARGO) test --workspace --locked

build:
	$(CARGO) build --workspace --locked

release:
	$(CARGO) build --release --locked --package workcell-mcp

ci: fmt-check check clippy test release

install:
	$(CARGO) install --path . --locked

run:
	@test -n "$(ROOT)" || { printf '%s\n' 'ROOT is required, for example: make run ROOT=/absolute/workspace'; exit 2; }
	$(CARGO) run --locked -- $(ARGS) "$(ROOT)"

run-web:
	$(CARGO) run --locked -- --tool-group web $(ARGS)

clean:
	$(CARGO) clean

docker-build:
	$(DOCKER) build --tag "$(IMAGE):$(TAG)" .

docker-smoke: docker-build
	$(DOCKER) run --rm "$(IMAGE):$(TAG)" --version

docker-run:
	@test -n "$(ROOT)" || { printf '%s\n' 'ROOT is required, for example: make docker-run ROOT=/absolute/workspace'; exit 2; }
	@test -n "$(WORKCELL_MCP_HTTP_TOKEN)" || { printf '%s\n' 'WORKCELL_MCP_HTTP_TOKEN is required'; exit 2; }
	$(DOCKER) run --rm \
		--read-only \
		--cap-drop ALL \
		--security-opt no-new-privileges \
		--pids-limit 128 \
		--tmpfs /tmp:rw,noexec,nosuid,size=16777216 \
		--mount "type=bind,src=$(abspath $(ROOT)),dst=/workspace" \
		--publish "127.0.0.1:$(PORT):3001" \
		--env WORKCELL_MCP_HTTP_TOKEN \
		"$(IMAGE):$(TAG)" \
		--transport http \
		--http-bind container \
		--allow-write \
		$(ARGS) \
		/workspace
