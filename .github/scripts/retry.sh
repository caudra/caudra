#!/usr/bin/env bash
# Retry a command with exponential backoff, for transient network failures.
for attempt in 1 2 3 4; do
  "$@" && exit 0
  echo "Attempt ${attempt} failed, retrying in $((2 ** attempt))s: $*" >&2
  sleep $((2 ** attempt))
done
exec "$@"
