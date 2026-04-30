#!/usr/bin/env bash
set -euo pipefail
# detect-port.sh — pick a single USB serial port for espflash, or fall through.
#
# `espflash`'s auto-detect can pick the wrong device on macOS when multiple
# `/dev/cu.*` entries exist (Bluetooth headsets, debug consoles, etc.) and
# fails with the generic "Error while connecting to device".  This helper
# narrows the candidate set to USB-attached serial devices (`usbmodem*`,
# `usbserial*` on macOS; `ttyUSB*`, `ttyACM*` on Linux) and only picks one
# when exactly one matches — otherwise it stays out of the way so espflash
# emits its own diagnostic.
#
# Usage:
#
#     port="$(scripts/detect-port.sh)"
#     [ -n "$port" ] && extra_args=(--port "$port")
#
# Honours $ESPFLASH_PORT — if it is set and non-empty, the script echoes it
# verbatim without scanning, so the env var override always wins.
#
# Helpful diagnostics are written to stderr; only the chosen port (or nothing)
# goes to stdout.

if [ -n "${ESPFLASH_PORT:-}" ]; then
    printf '%s' "$ESPFLASH_PORT"
    exit 0
fi

candidates=()
case "$(uname -s)" in
    Darwin)
        # `/dev/cu.*` is the call-up (non-blocking) variant — what espflash
        # should use.  Most setups expose both `cu.foo` and `tty.foo` for the
        # same physical device; fall back to `tty.*` only when no `cu.*`
        # candidates exist to avoid double-counting one device as two ports.
        for pattern in /dev/cu.usbmodem* /dev/cu.usbserial* /dev/cu.SLAB_USBtoUART* /dev/cu.wchusbserial*; do
            [ -e "$pattern" ] && candidates+=("$pattern")
        done
        if [ ${#candidates[@]} -eq 0 ]; then
            for pattern in /dev/tty.usbmodem* /dev/tty.usbserial* /dev/tty.SLAB_USBtoUART* /dev/tty.wchusbserial*; do
                [ -e "$pattern" ] && candidates+=("$pattern")
            done
        fi
        ;;
    Linux)
        for pattern in /dev/ttyUSB* /dev/ttyACM*; do
            [ -e "$pattern" ] && candidates+=("$pattern")
        done
        ;;
    *)
        exit 0
        ;;
esac

case ${#candidates[@]} in
    1)
        printf '%s' "${candidates[0]}"
        ;;
    0)
        printf 'Note: no USB serial port detected; espflash will scan all serial devices.\n' >&2
        printf '      If connection fails, set ESPFLASH_PORT=/dev/cu.usbmodemXXXX (macOS) or /dev/ttyUSBn (Linux).\n' >&2
        ;;
    *)
        printf 'Note: multiple USB serial ports detected; espflash auto-detect may pick the wrong one:\n' >&2
        for cand in "${candidates[@]}"; do
            printf '        %s\n' "$cand" >&2
        done
        printf '      Set ESPFLASH_PORT=<one of the above> to disambiguate.\n' >&2
        ;;
esac
