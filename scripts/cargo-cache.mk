# Routes every Cargo child of the root and Workcell Makefiles through sccache when it is installed.
# Worktrees keep private targets and share one bounded cache. `SCCACHE=` opts out, `SCCACHE=<path>`
# picks a binary, and any explicitly set Cargo wrapper variable, even an empty one, wins.
#
# Client-side mode is upstream's recommended mode: compilers run in the client and the daemon only
# serves storage. Preprocessor (direct) mode stays off because it can reuse a stale C object when a
# previously missing header appears; object caching for C and C++ still applies through cc-rs.
# The daemon reads SCCACHE_CACHE_SIZE when it starts, so changing it needs `sccache --stop-server`
# once no build is running.

CARGO_WRAPPER_VARIABLES := RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER CARGO_BUILD_RUSTC_WRAPPER \
	CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER

# Detected by name rather than path: Cargo resolves it on the same PATH, and a bare name survives
# Git Bash's POSIX-to-Windows path conversion.
ifeq ($(origin SCCACHE),undefined)
SCCACHE := $(if $(shell command -v sccache 2>/dev/null),sccache)
endif

ifneq ($(SCCACHE),)
ifeq ($(strip $(foreach variable,$(CARGO_WRAPPER_VARIABLES),$(filter-out undefined,$(origin $(variable))))),)
export RUSTC_WRAPPER := $(SCCACHE)
export SCCACHE_CACHE_SIZE ?= 40G
export SCCACHE_CLIENT_SIDE ?= 1
export SCCACHE_DIRECT ?= false
endif
endif
