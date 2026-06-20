#!/usr/bin/env bash
set -euo pipefail
# espflash.sh — run an espflash subcommand against the auto-detected USB serial port
# Usage: scripts/espflash.sh <subcommand> [extra-args...]
#   e.g. scripts/espflash.sh monitor --non-interactive
#        scripts/espflash.sh erase-flash
#
# The port is chosen by scripts/detect-port.sh (honours $ESPFLASH_PORT, otherwise
# narrows to USB serial devices so paired Bluetooth ports are not picked). When no
# port is found, espflash falls back to its own auto-detect.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ "$#" -lt 1 ]; then
    printf 'Usage: %s <espflash-subcommand> [extra-args...]\n' "$0" >&2
    exit 2
fi

subcommand="$1"
shift

port="$("$SCRIPT_DIR/detect-port.sh")"
port_args=()
[ -n "$port" ] && port_args=(--port "$port")

exec espflash "$subcommand" "${port_args[@]}" "$@"
