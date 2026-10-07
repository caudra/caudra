#!/usr/bin/env bash
#
# Compares this pipeline against the upstream ripwire binary it was ported from.
#
# The comparison is deliberately asymmetric. ripwire keeps an on-disk cache under
# /tmp/ripwire-<uid>, so its steady state is largely a cache hit; this crate has no persistent
# cache at all. The first ripwire column is therefore a standing target rather than a like-for-like
# result, and the second is what the same pipeline costs doing the same work.
#
# The cache is the whole footgun: without the rm below, ripwire's "cold" column silently becomes its
# warm one and the comparison inverts. That is not hypothetical — it happened, and it reported a 27x
# gap that did not exist.
#
#   cargo build --release --example code_map_bench -p workcell-mcp-code-graph
#   crates/mcp-code-graph/evals/compare-ripwire.sh <tree> [<tree> ...]
#
# RUNS=n overrides the repetition count. Medians are reported; a single run measures page-cache
# state as much as anything else.

set -uo pipefail

RUNS=${RUNS:-7}
BENCH=${BENCH:-./target/release/examples/code_map_bench}
CACHE=${RIPWIRE_CACHE:-/tmp/ripwire-$(id -u)}

if ! command -v ripwire >/dev/null; then
  printf '%s\n' 'ripwire is not on PATH; nothing to compare against' >&2
  exit 2
fi
if [[ ! -x "$BENCH" ]]; then
  printf '%s\n' "$BENCH is missing; build it with:" >&2
  printf '%s\n' '  cargo build --release --example code_map_bench -p workcell-mcp-code-graph' >&2
  exit 2
fi

median() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

elapsed_ms() {
  local start finish
  start=$(date +%s%N)
  "$@" >/dev/null 2>&1
  finish=$(date +%s%N)
  printf '%s' $(((finish - start) / 1000000))
}

for tree in "$@"; do
  printf '### %s (%s files)\n' "$tree" "$(find "$tree" -type f | wc -l)"

  cleared=()
  for _ in $(seq "$RUNS"); do
    rm -rf "$CACHE"
    cleared+=("$(elapsed_ms ripwire "$tree")")
  done

  # One warming run, discarded, so the measured runs are steady state rather than the miss that
  # populates the cache.
  ripwire "$tree" >/dev/null 2>&1
  warm=()
  for _ in $(seq "$RUNS"); do
    warm+=("$(elapsed_ms ripwire "$tree")")
  done

  printf '  ripwire, cache warm     median %s ms   [%s]\n' "$(median "${warm[@]}")" "${warm[*]}"
  printf '  ripwire, cache cleared  median %s ms   [%s]\n' "$(median "${cleared[@]}")" "${cleared[*]}"
  "$BENCH" "$tree" "$RUNS" | sed 's/^/  /'
  printf '\n'
done
