#!/bin/sh
# Docker maps localhost to ::1 as well as 127.0.0.1, so Executor's daemon listens on ::1 while
# `executor mcp` dials 127.0.0.1 and gives up on it. One address for the name settles both.
set -eu
hosts=$(grep -v '^::1' /etc/hosts)
printf '%s\n' "$hosts" >/etc/hosts
exec executor mcp
