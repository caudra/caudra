#!/usr/bin/env bash

set -euo pipefail

readonly workcell=${WORKCELL_LOCAL:-}
readonly project_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
readonly source_lockfile="$project_root/Cargo.lock"

if [[ -z "$workcell" || ! -f "$workcell/Cargo.toml" ]]; then
    exec cargo "$@"
fi

workcell_package=$workcell
if [[ -f "$workcell/crates/workcell/Cargo.toml" ]]; then
    workcell_package=$workcell/crates/workcell
fi
readonly workcell_package
printf -v patch 'patch."https://github.com/tensorninja/workcell-mcp".workcell.path="%s"' "$workcell_package"
readonly patch
lock_dir=$(mktemp -d "${TMPDIR:-/tmp}/caudra-cargo.XXXXXX")
readonly lock_dir
readonly local_lock="$lock_dir/Cargo.lock"

cleanup() {
    local command_status=$?

    trap - EXIT
    rm -rf "$lock_dir"
    exit "$command_status"
}

trap cleanup EXIT
cp "$source_lockfile" "$local_lock"
printf -v lock_config 'resolver.lockfile-path="%s"' "$local_lock"
readonly lock_config
cargo --config "$patch" --config "$lock_config" update --quiet -p workcell
if [[ ${1:-} == clippy ]]; then
    shift
    cargo clippy --config "$patch" --config "$lock_config" "$@"
elif [[ ${1:-} == nextest && ${2:-} == run ]]; then
    shift 2
    cargo nextest run --config "$patch" --config "$lock_config" "$@"
else
    cargo --config "$patch" --config "$lock_config" "$@"
fi
