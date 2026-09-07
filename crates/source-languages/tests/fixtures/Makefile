# Make fixture. Make is a code lane: targets are functions, assignments are
# constants, and prerequisites plus $(MAKE)/$(call ...) are call references.

include config.mk

CARGO := cargo
PROFILE ?= release
CRATES = crates
GIT_SHA != git rev-parse --short HEAD

define announce
	@echo "==> $(1)"
endef

.PHONY: all build test lint docs clean

all: build test lint

build:
	$(CARGO) build --profile $(PROFILE)

test: build
	$(CARGO) test --workspace

lint: build
	$(call announce,clippy)
	$(CARGO) clippy --all-targets -- -D warnings

docs:
	$(MAKE) -C docs html

clean:
	$(CARGO) clean

check: test lint docs
	$(MAKE) build
