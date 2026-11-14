#!/usr/bin/env bash
set -euo pipefail
# ramdisk-mounted.sh — print "true" if <path> is a live mounted volume, else "false".
# Used by the justfile via `shell(...)` to decide between RAM-disk and fallback target dirs.
# Always exits 0 so a non-Darwin host or missing path produces "false" without aborting Just.
# Canonical copy: rustyfarian-ci/scripts/ (conventions/just.md); repos copy it byte for byte.

if [ $# -lt 1 ]; then
    printf 'Usage: %s <path>\n' "$0" >&2
    exit 2
fi

if [ "$(uname)" != "Darwin" ]; then
    echo false
    exit 0
fi

# `path_exists` would be fooled by a stale /Volumes/<name> directory that was never attached.
if [ -n "$1" ] && diskutil info "$1" >/dev/null 2>&1; then
    echo true
else
    echo false
fi
