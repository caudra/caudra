#!/usr/bin/env bash
set -euo pipefail

DEFAULT_PAGE_SIZE=25

normalize() {
	local sku="$1"
	printf '%s' "${sku^^}"
}

catalog_add() {
	local sku normalized
	sku="$1"
	normalized="$(normalize "$sku")"
	printf '%s=%s\n' "$normalized" "$2"
}

build_catalog() {
	catalog_add "a-1" "$DEFAULT_PAGE_SIZE"
}

build_catalog
