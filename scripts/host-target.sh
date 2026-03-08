#!/usr/bin/env bash
set -euo pipefail
# host-target.sh — determine the Rust host target triple
# Usage: scripts/host-target.sh (no arguments)

host=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
if [ -z "$host" ]; then
    printf 'Error: Failed to determine rustc host target. Is rustc installed?\n' >&2
    exit 1
fi
echo "$host"
