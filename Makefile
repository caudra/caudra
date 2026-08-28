SHELL := /bin/bash
.DEFAULT_GOAL := ci

CARGO ?= cargo
DOCKER ?= docker
IMAGE ?= workcell-mcp
TAG ?= local
ROOT ?=
PORT ?= 3001
ARGS ?=
# Must match the `monty-pool` pin in Cargo.toml; `code-worker` enforces it.
MONTY_VERSION ?= 0.0.21
CODE_WORKER_ROOT ?= target/code-worker
CODE_WORKER_BUILD ?= target/code-worker-build
CODE_WORKER ?= $(CODE_WORKER_ROOT)/bin/monty

.PHONY: help code-worker fmt fmt-check check clippy test build release ci install run run-web clean docker-build docker-smoke docker-run

help:
	@printf '%s\n' \
		'Workcell MCP targets:' \
		'  make code-worker   Install the pinned monty worker binary for the code tool group' \
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

# The code tool group executes snippets in a separate `monty` worker process.
#
# Installed rather than built as a workspace member for two reasons. `--locked` builds it against
# Monty's own published lockfile, which is the only resolution its large ruff/ty dependency tree is
# known to satisfy. And keeping it out of the workspace stops feature unification from enabling
# `monty-proto/worker` for the server too, which would link the interpreter into the binary we ship.
#
# `--no-default-features` drops Monty's standalone CLI, leaving a binary that only serves
# `monty subprocess` and cannot run a REPL, a file, or `-c`.
code-worker:
	@lock_version=$$(awk '/^name = "monty-pool"$$/ { getline; gsub(/[",]/, "", $$3); print $$3 }' Cargo.lock); \
	if [[ "$$lock_version" != "$(MONTY_VERSION)" ]]; then \
		printf '%s\n' "monty-pool is pinned to $$lock_version but MONTY_VERSION is $(MONTY_VERSION);" \
			'the worker protocol is version-coupled, so update both together'; \
		exit 2; \
	fi
	$(CARGO) install monty-runtime --version "=$(MONTY_VERSION)" --locked --no-default-features \
		--root "$(CODE_WORKER_ROOT)" --target-dir "$(CODE_WORKER_BUILD)"
	@# The server finds the worker beside its own executable, which is the same rule the container
	@# relies on. Placing a copy in each Cargo profile directory makes `cargo run` and a direct
	@# `./target/<profile>/workcell-mcp` work with no configuration, exactly like the image.
	@for profile in debug release; do \
		mkdir -p "target/$$profile"; \
		install -m 0755 "$(CODE_WORKER)" "target/$$profile/monty"; \
	done
	@printf '%s\n' 'installed $(CODE_WORKER) and copied it beside the debug and release binaries'

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all --check

check:
	$(CARGO) check --workspace --all-targets --locked

clippy:
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

# Code-group tests locate $(CODE_WORKER) themselves and skip when it is absent, so `make
# code-worker` is what makes them run. Deliberately not exported here: WORKCELL_MCP_CODE_WORKER is
# operator configuration, and setting it process-wide would leak into tests that assert how the CLI
# reacts to it being present.
test:
	$(CARGO) test --workspace --locked

build:
	$(CARGO) build --workspace --locked

release:
	$(CARGO) build --release --locked --package workcell-mcp

ci: fmt-check check clippy test release

# Installs the worker into the same Cargo bin directory, so the installed server finds it beside
# itself. Without this the code tool group would be enabled but unusable after a plain install.
install:
	$(CARGO) install --path . --locked
	$(CARGO) install monty-runtime --version "=$(MONTY_VERSION)" --locked --no-default-features

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
	@printf '%s\n' 'checking the image ships a usable code execution worker'
	$(DOCKER) run --rm --entrypoint /usr/local/bin/monty "$(IMAGE):$(TAG)" -c 'print(1)' 2>&1 \
		| grep -q 'subprocess' \
		|| { printf '%s\n' 'monty worker is missing or was built with the standalone CLI'; exit 2; }

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
