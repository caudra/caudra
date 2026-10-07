SHELL := /bin/bash
.DEFAULT_GOAL := ci

CARGO ?= cargo
DOCKER ?= docker
PYTHON ?= python3
REPO_ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST)))/..)
export CARGO_TARGET_DIR := $(REPO_ROOT)/target
IMAGE ?= workcell-mcp
TAG ?= local
ROOT ?=
PORT ?= 3001
ARGS ?=
CODE_WORKER_ROOT ?= $(CARGO_TARGET_DIR)/code-worker
CODE_WORKER_BUILD ?= $(CARGO_TARGET_DIR)/code-worker-build
CODE_WORKER ?= $(CODE_WORKER_ROOT)/bin/monty
BUNDLED_CODE_WORKER_ENV := WORKCELL_BUNDLED_MONTY_WORKER

.PHONY: help code-worker fmt fmt-check check check-native clippy test test-optional-features build release release-fast ci install run run-web clean docker-build docker-smoke docker-run

# Cargo wants one comma-separated `--features` value; Make can only build that from a word list.
comma := ,
empty :=
space := $(empty) $(empty)
# Tool groups exposed by the `workcell` embedding facade, each gating one optional dependency.
NATIVE_GROUPS := files files-index web shell code code-graph code-graph-git environment scm snapshots snapshot-store
NATIVE_FEATURES := $(subst $(space),$(comma),$(NATIVE_GROUPS))
# Tool crates that must compile with no MCP adapter, so native hosts never link a transport.
NATIVE_CRATES := workcell-mcp-files workcell-mcp-web workcell-mcp-shell workcell-mcp-code workcell-mcp-code-graph workcell-environment workcell-workspace-scm workcell-workspace-snapshot workcell-snapshot-store

help:
	@printf '%s\n' \
		'Workcell MCP targets:' \
		'  make code-worker   Install the pinned monty worker binary for the code tool group' \
		'  make fmt           Format the Rust workspace' \
		'  make fmt-check     Verify formatting without changing files' \
		'  make check         Type-check all Workcell targets' \
		'  make check-native  Type-check the protocol-neutral facade without any MCP adapter' \
		'  make clippy        Run Clippy with warnings denied' \
		'  make test          Run all Workcell tests with the root lockfile' \
		'  make build         Build the debug server with the root lockfile' \
		'  make release       Build the optimized binary with the lockfile' \
		'  make release-fast  Build an optimized binary without LTO for local iteration' \
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
#
# The root `scripts/build-code-worker.py` fixes the worker's profile and install command instead of taking
# Cargo's defaults, so every checkout builds the same worker and a shared CODE_WORKER_BUILD reuses
# compiled artifacts. It publishes a stripped `bin/monty`, keeps the unstripped build at
# `symbols/bin/monty` for diagnostics, and rebuilds when the helper, toolchain, target, or RUSTFLAGS
# no longer match its stamp. Any other CODE_WORKER belongs to the caller: it is version-checked,
# never built or stripped.
code-worker:
	@expected=$$($(PYTHON) "$(REPO_ROOT)/scripts/build-code-worker.py" --check-version) || exit 2; \
	if [[ "$(CODE_WORKER)" == "$(CODE_WORKER_ROOT)/bin/monty" ]]; then \
		$(PYTHON) "$(REPO_ROOT)/scripts/build-code-worker.py" \
			--root "$(CODE_WORKER_ROOT)" --target-dir "$(CODE_WORKER_BUILD)"; \
	elif ! "$(CODE_WORKER)" --version 2>&1 | grep -qxF "$$expected"; then \
		printf '%s\n' "CODE_WORKER=$(CODE_WORKER) must be an executable reporting $$expected"; \
		exit 2; \
	fi
	@# The server finds the worker beside its own executable, which is the same rule the container
	@# relies on. Placing a copy in each Cargo profile directory makes `cargo run` and a direct
	@# `./target/<profile>/workcell-mcp` work with no configuration, exactly like the image.
	@for profile in debug release release-fast; do \
		mkdir -p "$(CARGO_TARGET_DIR)/$$profile"; \
		install -m 0755 "$(CODE_WORKER)" "$(CARGO_TARGET_DIR)/$$profile/monty"; \
	done
	@printf '%s\n' 'copied $(CODE_WORKER) beside the debug, release, and release-fast binaries'

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all --check

check:
	$(CARGO) check --package 'workcell*' --all-targets --locked

# `check` cannot cover the native path. Cargo unifies features across a workspace build, and the
# standalone server depends on every tool crate with `features = ["mcp"]`, so `--workspace` always
# resolves the MCP adapter in. The facade's own `default = []` then leaves it compiling empty. Both
# effects together mean no workspace-wide command ever builds a tool crate without `rmcp`, which is
# precisely the configuration native hosts consume. These per-package checks are that coverage.
check-native:
	@for group in $(NATIVE_GROUPS); do \
		printf '%s\n' "checking facade group: $$group"; \
		$(CARGO) check --locked --package workcell --no-default-features --features "$$group" || exit 1; \
	done
	$(CARGO) check --locked --package workcell --no-default-features --features code-bundled
	$(CARGO) check --locked --package workcell --no-default-features \
		--features "$(NATIVE_FEATURES)"
	@for crate in $(NATIVE_CRATES); do \
		printf '%s\n' "checking crate without MCP adapter: $$crate"; \
		$(CARGO) check --locked --package "$$crate" --no-default-features || exit 1; \
	done
	$(CARGO) clippy --locked --package workcell --no-default-features \
		--features "$(NATIVE_FEATURES)" -- -D warnings
	$(CARGO) clippy --locked --package workcell --no-default-features \
		--features code-bundled -- -D warnings
	@# A non-optional rmcp dependency anywhere in the tool crates would silently pull a transport
	@# into every native host. Assert its absence rather than trusting the feature declarations.
	@tree=$$($(CARGO) tree --locked --package workcell --no-default-features \
		--features "$(NATIVE_FEATURES)" --prefix none) || exit 1; \
	if printf '%s\n' "$$tree" | grep -q '^rmcp '; then \
		printf '%s\n' 'rmcp is reachable from the protocol-neutral facade; an MCP dependency is no longer optional'; \
		exit 2; \
	fi
	@tree=$$($(CARGO) tree --locked --package workcell --no-default-features \
		--features code-bundled --prefix none) || exit 1; \
	if printf '%s\n' "$$tree" | grep -q '^rmcp '; then \
		printf '%s\n' 'rmcp is reachable from the bundled native code facade'; \
		exit 2; \
	fi
	@tree=$$($(CARGO) tree --locked --package workcell --no-default-features \
		--features files --prefix none) || exit 1; \
	if printf '%s\n' "$$tree" | grep -Eq '^(mlua|tree-sitter)[ -]'; then \
		printf '%s\n' 'the base files facade reaches the optional index parser bundle'; \
		exit 2; \
	fi
	@tree=$$($(CARGO) tree --locked --package workcell --no-default-features \
		--features files-index --prefix none) || exit 1; \
	if printf '%s\n' "$$tree" | grep -q '^mlua '; then \
		printf '%s\n' 'the native index facade reaches a Lua interpreter'; \
		exit 2; \
	fi
	@test ! -e crates/mcp-files/src/index/lua.rs \
		&& test ! -e crates/mcp-files/src/index/lua/indexer.lua \
		|| { printf '%s\n' 'the native index contains bundled Lua implementation assets'; exit 2; }
	@printf '%s\n' 'native facade builds for every tool group with no MCP dependency'

clippy:
	$(CARGO) clippy --package 'workcell*' --all-targets --locked -- -D warnings

# Build-time bundling is separate from the runtime override, so tests exercise both embedded and
# explicit-path worker sources without leaking operator configuration into CLI tests.
test: code-worker test-optional-features
	$(PYTHON) "$(REPO_ROOT)/scripts/test-build-code-worker.py"
	WORKCELL_REQUIRE_CODE_WORKER=1 $(BUNDLED_CODE_WORKER_ENV)="$(abspath $(CODE_WORKER))" \
		$(CARGO) test --package 'workcell*' --locked

# Optional features are dark to `--workspace` until something in the workspace turns them on, and a
# feature nothing exercises is a feature nothing protects. These run the crates that carry one.
test-optional-features:
	$(CARGO) test --locked --package workcell-code-graph --features git
	$(CARGO) clippy --locked --all-targets --package workcell-code-graph --features git \
		-- -D warnings
	$(CARGO) test --locked --package workcell-mcp-code-graph --features git
	$(CARGO) clippy --locked --all-targets --package workcell-mcp-code-graph --features git \
		-- -D warnings

build:
	$(CARGO) build --package workcell-mcp --locked

release: code-worker
	$(BUNDLED_CODE_WORKER_ENV)="$(abspath $(CODE_WORKER))" \
		$(CARGO) build --release --locked --package workcell-mcp

release-fast: code-worker
	$(BUNDLED_CODE_WORKER_ENV)="$(abspath $(CODE_WORKER))" \
		$(CARGO) build --profile release-fast --locked --package workcell-mcp

ci: code-worker fmt-check check check-native clippy test release

# The installed server carries the bundled worker; build input is not a runtime override.
install: code-worker
	$(BUNDLED_CODE_WORKER_ENV)="$(abspath $(CODE_WORKER))" $(CARGO) install --path . --locked

run: code-worker
	@test -n "$(ROOT)" || { printf '%s\n' 'ROOT is required, for example: make run ROOT=/absolute/workspace'; exit 2; }
	$(CARGO) run --locked --package workcell-mcp -- $(ARGS) "$(ROOT)"

run-web:
	$(CARGO) run --locked --package workcell-mcp -- --tool-group web $(ARGS)

clean:
	$(CARGO) clean

docker-build:
	$(DOCKER) build --file "$(REPO_ROOT)/workcell/Dockerfile" --tag "$(IMAGE):$(TAG)" "$(REPO_ROOT)"

docker-smoke: docker-build
	$(DOCKER) run --rm "$(IMAGE):$(TAG)" --version
	@printf '%s\n' 'executing code through the packaged MCP server and adjacent worker'
	@output=$$(printf '%s\n' \
		'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"container-smoke","version":"1"}}}' \
		'{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' \
		'{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"python_execution","arguments":{"code":"sum([1, 2, 3, 4])"}}}' \
		| $(DOCKER) run --rm --interactive "$(IMAGE):$(TAG)" --tool-group python_execution); \
	printf '%s\n' "$$output" | grep -q '"outcome":"completed".*"result":10' \
		|| { printf '%s\n' 'packaged code execution failed'; exit 2; }
	@printf '%s\n' 'discovering and executing index through the packaged MCP server'
	@output=$$(printf '%s\n' \
		'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"container-smoke","version":"1"}}}' \
		'{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' \
		'{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
		'{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"file_index","arguments":{"path":"LICENSE.md"}}}' \
		| $(DOCKER) run --rm --interactive "$(IMAGE):$(TAG)" --tool-group files \
			/usr/share/doc/workcell-mcp); \
	printf '%s\n' "$$output" | grep -q '"name":"file_index"' \
		|| { printf '%s\n' 'packaged index catalog is incomplete'; exit 2; }; \
	printf '%s\n' "$$output" | grep -q '"kind":"file".*"language":"markdown"' \
		|| { printf '%s\n' 'packaged index execution failed'; exit 2; }
	@printf '%s\n' 'ranking a tree through the packaged MCP server'
	@output=$$(printf '%s\n' \
		'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"container-smoke","version":"1"}}}' \
		'{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}' \
		'{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' \
		'{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"code_map","arguments":{}}}' \
		| $(DOCKER) run --rm --interactive "$(IMAGE):$(TAG)" --tool-group code_graph \
			/usr/share/doc/workcell-mcp); \
	printf '%s\n' "$$output" | grep -q '"name":"code_map"' \
		|| { printf '%s\n' 'packaged code-graph catalog is incomplete'; exit 2; }; \
	printf '%s\n' "$$output" | grep -q '"graph":' \
		|| { printf '%s\n' 'packaged code-graph execution failed'; exit 2; }

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
